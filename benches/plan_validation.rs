//! Disposable A/B probes for the post-#188 optimization plan.
//!
//! These deliberately keep the baseline and prototype in one binary so both
//! see identical codegen, machine load, and thermal conditions.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::hint::black_box;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use kube::core::DynamicObject;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use sofka::benchsupport as bs;

#[derive(Deserialize)]
struct Selected<'a> {
    #[serde(borrow, rename = "_time")]
    time: Cow<'a, str>,
    #[serde(borrow, rename = "_msg")]
    msg: Cow<'a, str>,
    #[serde(borrow, rename = "kubernetes.pod_name", default)]
    pod: Cow<'a, str>,
    #[serde(borrow, rename = "kubernetes.container_name", default)]
    container: Cow<'a, str>,
}

struct Retained {
    time: String,
    msg: String,
    pod: String,
    container: String,
}

fn retain(selected: Selected<'_>) -> Retained {
    Retained {
        time: selected.time.into_owned(),
        msg: selected.msg.trim_end_matches('\n').to_owned(),
        pod: selected.pod.into_owned(),
        container: selected.container.into_owned(),
    }
}

fn parse_dom(line: &str) -> Option<Retained> {
    let value: Value = serde_json::from_str(line).ok()?;
    Some(Retained {
        time: value.get("_time")?.as_str()?.to_owned(),
        msg: value
            .get("_msg")?
            .as_str()?
            .trim_end_matches('\n')
            .to_owned(),
        pod: value
            .get("kubernetes.pod_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        container: value
            .get("kubernetes.container_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn parse_selective(line: &str) -> Option<Retained> {
    serde_json::from_str::<Selected<'_>>(line).ok().map(retain)
}

fn checksum(entries: &[Retained]) -> usize {
    entries
        .iter()
        .map(|e| e.time.len() + e.msg.len() + e.pod.len() + e.container.len())
        .sum()
}

fn provider_lines(n: usize, message_len: usize) -> Vec<String> {
    let payload = "x".repeat(message_len);
    (0..n)
        .map(|i| {
            format!(
                r#"{{"_time":"2026-09-04T10:00:{:02}Z","_msg":"message-{i}-{payload}","kubernetes.pod_name":"api-{i}","kubernetes.container_name":"app","ignored":{{"nested":[1,2,3],"label":"duplicate"}},"level":"info"}}"#,
                i % 60
            )
        })
        .collect()
}

fn provider_parse(c: &mut Criterion) {
    let mut g = c.benchmark_group("plan/provider_parse");
    for (n, message_len) in [(1_000, 32), (1_000, 4_096), (10_000, 32)] {
        let lines = provider_lines(n, message_len);
        let id = format!("{n}x{message_len}");
        g.bench_with_input(BenchmarkId::new("value_dom", &id), &lines, |b, lines| {
            b.iter(|| {
                let entries: Vec<_> = lines.iter().filter_map(|line| parse_dom(line)).collect();
                black_box(checksum(&entries))
            })
        });
        g.bench_with_input(
            BenchmarkId::new("selective_serde", &id),
            &lines,
            |b, lines| {
                b.iter(|| {
                    let entries: Vec<_> = lines
                        .iter()
                        .filter_map(|line| parse_selective(line))
                        .collect();
                    black_box(checksum(&entries))
                })
            },
        );
        g.bench_with_input(BenchmarkId::new("simd_json", &id), &lines, |b, lines| {
            b.iter(|| {
                let mut sum = 0usize;
                for line in lines {
                    // simd-json requires mutable padded input; network chunks
                    // are mutable, but cloning here prices the ownership cost
                    // a line-oriented integration would actually pay.
                    let mut bytes = line.as_bytes().to_vec();
                    if let Ok(e) = simd_json::serde::from_slice::<Selected<'_>>(&mut bytes) {
                        sum += checksum(&[retain(e)]);
                    }
                }
                black_box(sum)
            })
        });
    }
    g.finish();
}

fn drain_baseline(buf: &mut Vec<u8>) -> Vec<String> {
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    let complete: Vec<u8> = buf.drain(..=last_nl).collect();
    String::from_utf8_lossy(&complete)
        .lines()
        .map(str::to_string)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn ingest_baseline(chunks: &[Vec<u8>]) -> usize {
    let mut buf = Vec::new();
    let mut sum = 0usize;
    for chunk in chunks {
        buf.extend_from_slice(chunk);
        for line in drain_baseline(&mut buf) {
            if let Some(entry) = parse_dom(&line) {
                sum += checksum(&[entry]);
            }
        }
    }
    sum
}

fn ingest_in_place(chunks: &[Vec<u8>]) -> usize {
    let mut buf = Vec::new();
    let mut sum = 0usize;
    for chunk in chunks {
        buf.extend_from_slice(chunk);
        let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if complete == 0 {
            continue;
        }
        for line in buf[..complete].split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(line) = std::str::from_utf8(line)
                && let Some(entry) = parse_selective(line)
            {
                sum += checksum(&[entry]);
            }
        }
        buf.copy_within(complete.., 0);
        buf.truncate(buf.len() - complete);
    }
    sum
}

fn chunked(bytes: &[u8], chunk: usize) -> Vec<Vec<u8>> {
    bytes.chunks(chunk).map(<[u8]>::to_vec).collect()
}

fn provider_framing(c: &mut Criterion) {
    let mut wire = provider_lines(10_000, 48).join("\n").into_bytes();
    wire.push(b'\n');
    let mut g = c.benchmark_group("plan/provider_framing");
    for size in [1_024usize, 65_536] {
        let chunks = chunked(&wire, size);
        g.bench_with_input(BenchmarkId::new("baseline", size), &chunks, |b, chunks| {
            b.iter(|| black_box(ingest_baseline(chunks)))
        });
        g.bench_with_input(BenchmarkId::new("in_place", size), &chunks, |b, chunks| {
            b.iter(|| black_box(ingest_in_place(chunks)))
        });
    }
    let fragmented = chunked(&wire, 17);
    g.bench_function("baseline_fragmented17", |b| {
        b.iter(|| black_box(ingest_baseline(&fragmented)))
    });
    g.bench_function("in_place_fragmented17", |b| {
        b.iter(|| black_box(ingest_in_place(&fragmented)))
    });
    g.finish();
}

fn render_frame(c: &mut Criterion) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut g = c.benchmark_group("plan/render_frame_2000_120x52");
    for (label, filter) in [("plain", ""), ("filtered", "workload-01")] {
        let (mut app, _rx) = bs::pods_app(2_000);
        app.filter = filter.to_owned();
        let mut terminal = Terminal::new(TestBackend::new(120, 52)).unwrap();
        terminal
            .draw(|frame| sofka::ui::draw(frame, &mut app))
            .unwrap();
        g.bench_function(label, |b| {
            b.iter(|| {
                terminal
                    .draw(|frame| sofka::ui::draw(frame, &mut app))
                    .unwrap();
                black_box(&terminal);
            })
        });
    }
    g.finish();
}

struct RowIdentity {
    namespace: String,
    name: String,
    key: String,
    resource_version: String,
}

fn row_identity(c: &mut Criterion) {
    let rows: Vec<_> = (0..40)
        .map(|i| {
            let namespace = format!("namespace-{}", i % 12);
            let name = format!("workload-{i:05}-7d9f8b6c5d");
            RowIdentity {
                key: format!("{namespace}/{name}"),
                namespace,
                name,
                resource_version: format!("{}", 100_000 + i),
            }
        })
        .collect();
    let cache: HashMap<_, _> = rows
        .iter()
        .map(|row| (row.key.clone(), row.resource_version.clone()))
        .collect();
    let mut g = c.benchmark_group("plan/row_identity_40");
    g.bench_function("format_clone_current", |b| {
        b.iter(|| {
            let mut sum = 0usize;
            for row in &rows {
                let cache_key = format!("{}/{}", row.namespace, row.name);
                let rv = row.resource_version.clone();
                sum += cache.get(&cache_key).is_some_and(|cached| cached == &rv) as usize;
                let mark_key = format!("{}/{}", row.namespace, row.name);
                let metrics_key = format!("{}/{}", row.namespace, row.name);
                let name = row.name.clone();
                sum += mark_key.len() + metrics_key.len() + name.len();
            }
            black_box(sum)
        })
    });
    g.bench_function("canonical_borrowed", |b| {
        b.iter(|| {
            let mut sum = 0usize;
            for row in &rows {
                sum += cache
                    .get(row.key.as_str())
                    .is_some_and(|cached| cached.as_str() == row.resource_version.as_str())
                    as usize;
                sum += row.key.len() * 2 + row.name.len();
            }
            black_box(sum)
        })
    });
    g.finish();
}

fn per_frame_derived_work(c: &mut Criterion) {
    use k8s_openapi::jiff::Timestamp;
    use ratatui::layout::{Constraint, Flex, Layout, Rect};

    let mut g = c.benchmark_group("plan/per_frame_derived");
    g.bench_function("timestamp_160_calls", |b| {
        b.iter(|| {
            let mut last = Timestamp::UNIX_EPOCH;
            for _ in 0..160 {
                last = Timestamp::now();
            }
            black_box(last)
        })
    });
    g.bench_function("timestamp_once", |b| b.iter(|| black_box(Timestamp::now())));

    let widths = [
        Constraint::Length(18),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(7),
    ];
    let area = Rect::new(2, 1, 116, 48);
    g.bench_function("ratatui_layout_for_hit_test", |b| {
        b.iter(|| {
            black_box(
                Layout::horizontal(widths)
                    .flex(Flex::Start)
                    .spacing(2)
                    .split(area),
            )
        })
    });
    g.bench_function("direct_hit_rects", |b| {
        b.iter(|| {
            let mut x = area.x;
            let rects: Vec<_> = [18u16, 12, 10, 8, 8, 7]
                .into_iter()
                .map(|width| {
                    let rect = (x, x + width);
                    x += width + 2;
                    rect
                })
                .collect();
            black_box(rects)
        })
    });
    g.finish();
}

fn highlight_baseline(name: &str, matched: &[usize]) -> usize {
    let matched: HashSet<usize> = matched.iter().copied().collect();
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    for (i, ch) in name.chars().enumerate() {
        let is_match = matched.contains(&i);
        if !run.is_empty() && is_match != run_matched {
            spans.push(std::mem::take(&mut run));
        }
        run_matched = is_match;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(run);
    }
    spans.iter().map(String::len).sum()
}

fn highlight_borrowed(name: &str, matched: &[usize]) -> usize {
    let mut wanted = matched.iter().copied().peekable();
    let mut run_start = 0usize;
    let mut prev_match = false;
    let mut total = 0usize;
    for (char_idx, (byte_idx, _)) in name.char_indices().enumerate() {
        let is_match = wanted.peek().is_some_and(|&idx| idx == char_idx);
        if is_match {
            wanted.next();
        }
        if byte_idx != 0 && is_match != prev_match {
            total += name[run_start..byte_idx].len();
            run_start = byte_idx;
        }
        prev_match = is_match;
    }
    total + name[run_start..].len()
}

fn highlights(c: &mut Criterion) {
    let matcher = SkimMatcherV2::default();
    let names: Vec<String> = (0..40)
        .map(|i| format!("workload-{i:05}-7d9f8b6c5d-abcd"))
        .collect();
    let indices: Vec<Vec<usize>> = names
        .iter()
        .map(|name| matcher.fuzzy_indices(name, "wld").unwrap().1)
        .collect();
    let mut g = c.benchmark_group("plan/highlight_40");
    g.bench_function("owned_hashset", |b| {
        b.iter(|| {
            black_box(
                names
                    .iter()
                    .zip(&indices)
                    .map(|(name, idx)| highlight_baseline(name, idx))
                    .sum::<usize>(),
            )
        })
    });
    g.bench_function("borrowed_sorted", |b| {
        b.iter(|| {
            black_box(
                names
                    .iter()
                    .zip(&indices)
                    .map(|(name, idx)| highlight_borrowed(name, idx))
                    .sum::<usize>(),
            )
        })
    });
    g.finish();
}

fn pickers_and_headers(c: &mut Criterion) {
    let (mut app, _rx) = bs::contexts_app(2_000);
    app.ctx_filter = "cluster-1".to_owned();
    let cached = app.filtered_contexts();
    let headers = app.display_headers();
    let mut g = c.benchmark_group("plan/derived_models");
    g.bench_function("contexts_recompute_2000", |b| {
        b.iter(|| black_box(app.filtered_contexts()))
    });
    g.bench_function("contexts_cached_2000", |b| b.iter(|| black_box(&cached)));
    g.bench_function("headers_recompute", |b| {
        b.iter(|| black_box(app.display_headers()))
    });
    g.bench_function("headers_cached", |b| b.iter(|| black_box(&headers)));
    g.finish();
}

fn custom_columns(c: &mut Criterion) {
    let pods: Vec<DynamicObject> = (0..2_000).map(bs::pod).collect();
    let paths = [
        ("scalar", "/status/phase"),
        ("array", "/status/containerStatuses"),
        ("object", "/status"),
    ];
    let mut g = c.benchmark_group("plan/custom_extract_2000");
    for (label, path) in paths {
        g.bench_function(BenchmarkId::new("owned", label), |b| {
            b.iter(|| {
                let mut sum = 0usize;
                for pod in &pods {
                    sum +=
                        sofka::views::extract(pod, path).map_or(0, |value| value.to_string().len());
                }
                black_box(sum)
            })
        });
        g.bench_function(BenchmarkId::new("borrowed", label), |b| {
            b.iter(|| {
                let mut sum = 0usize;
                for pod in &pods {
                    sum += pod
                        .data
                        .pointer(path)
                        .map_or(0, |value| value.to_string().len());
                }
                black_box(sum)
            })
        });
    }
    g.finish();
}

fn exercise_map<S: std::hash::BuildHasher>(keys: &[String], state: S) -> usize {
    let mut map = HashMap::with_capacity_and_hasher(keys.len(), state);
    for (i, key) in keys.iter().enumerate() {
        map.insert(key.as_str(), i);
    }
    keys.iter()
        .rev()
        .filter_map(|key| map.get(key.as_str()))
        .sum()
}

fn hashers(c: &mut Criterion) {
    let keys: Vec<String> = (0..20_000)
        .map(|i| format!("same-prefix-namespace/workload-{i:08}-7d9f8b6c5d"))
        .collect();
    let mut g = c.benchmark_group("plan/hash_build_lookup_20000");
    g.bench_function("std_random", |b| {
        b.iter(|| {
            black_box(exercise_map(
                &keys,
                std::collections::hash_map::RandomState::new(),
            ))
        })
    });
    g.bench_function("ahash_random", |b| {
        b.iter(|| black_box(exercise_map(&keys, ahash::RandomState::new())))
    });
    g.bench_function("foldhash_random", |b| {
        b.iter(|| black_box(exercise_map(&keys, foldhash::fast::RandomState::default())))
    });
    g.finish();
}

fn string_representations(c: &mut Criterion) {
    use compact_str::CompactString;
    use lasso::{Rodeo, Spur};

    let values: Vec<String> = (0..20_000)
        .map(|i| match i % 4 {
            0 => "Running".to_owned(),
            1 => "kube-system".to_owned(),
            2 => format!("node-{}", i % 80),
            _ => format!("namespace-{}", i % 24),
        })
        .collect();
    let mut g = c.benchmark_group("plan/string_representation_20000");
    g.bench_function("string_clone", |b| b.iter(|| black_box(values.clone())));
    g.bench_function("compact_str", |b| {
        b.iter(|| {
            black_box(
                values
                    .iter()
                    .map(|value| CompactString::new(value))
                    .collect::<Vec<_>>(),
            )
        })
    });
    g.bench_function("lasso_intern", |b| {
        b.iter(|| {
            let mut rodeo: Rodeo<Spur> = Rodeo::new();
            let ids: Vec<_> = values
                .iter()
                .map(|value| rodeo.get_or_intern(value))
                .collect();
            black_box((rodeo, ids))
        })
    });
    g.finish();
}

fn fuzzy_engines(c: &mut Criterion) {
    let names: Vec<String> = (0..20_000)
        .map(|i| format!("ns-{}/workload-{i:05}-7d9f8b6c5d", i % 24))
        .collect();
    let mut g = c.benchmark_group("plan/fuzzy_20000");
    let skim = SkimMatcherV2::default();
    g.bench_function("skim_matcher", |b| {
        b.iter(|| {
            black_box(
                names
                    .iter()
                    .filter_map(|name| skim.fuzzy_match(name, "wld199"))
                    .sum::<i64>(),
            )
        })
    });
    let pattern = Pattern::new(
        "wld199",
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut nucleo = Matcher::default();
    let mut utf32_buf = Vec::new();
    g.bench_function("nucleo_matcher", |b| {
        b.iter(|| {
            let mut score = 0u64;
            for name in &names {
                score += pattern
                    .score(Utf32Str::new(name, &mut utf32_buf), &mut nucleo)
                    .unwrap_or(0) as u64;
            }
            black_box(score)
        })
    });
    g.finish();
}

fn parallel_filter(c: &mut Criterion) {
    let names: Vec<String> = (0..100_000)
        .map(|i| format!("ns-{}/workload-{i:05}-7d9f8b6c5d", i % 24))
        .collect();
    let mut g = c.benchmark_group("plan/parallel_filter");
    for n in [2_000usize, 20_000, 100_000] {
        let slice = &names[..n];
        g.bench_with_input(BenchmarkId::new("sequential", n), &n, |b, _| {
            b.iter(|| black_box(slice.iter().filter(|s| s.contains("199")).count()))
        });
        g.bench_with_input(BenchmarkId::new("rayon", n), &n, |b, _| {
            b.iter(|| black_box(slice.par_iter().filter(|s| s.contains("199")).count()))
        });
    }
    g.finish();
}

struct AosRow {
    _name: String,
    status: String,
    _namespace: String,
    cpu: i64,
}

fn columnar_storage(c: &mut Criterion) {
    let rows: Vec<_> = (0..100_000)
        .map(|i| AosRow {
            _name: format!("workload-{i:08}"),
            status: if i % 7 == 0 { "Pending" } else { "Running" }.to_owned(),
            _namespace: format!("namespace-{}", i % 24),
            cpu: (i % 2_000) as i64,
        })
        .collect();
    let statuses: Vec<_> = rows.iter().map(|row| row.status.as_str()).collect();
    let cpus: Vec<_> = rows.iter().map(|row| row.cpu).collect();
    let mut g = c.benchmark_group("plan/columnar_scan_100000");
    g.bench_function("array_of_structs", |b| {
        b.iter(|| {
            black_box(
                rows.iter()
                    .filter(|row| row.status == "Running" && row.cpu > 1_000)
                    .count(),
            )
        })
    });
    g.bench_function("struct_of_arrays", |b| {
        b.iter(|| {
            black_box(
                statuses
                    .iter()
                    .zip(&cpus)
                    .filter(|(status, cpu)| **status == "Running" && **cpu > 1_000)
                    .count(),
            )
        })
    });
    g.finish();
}

fn message_queue(c: &mut Criterion) {
    use sofka::store::Msg;
    use std::collections::VecDeque;

    let mut g = c.benchmark_group("plan/message_queue_4096");
    g.bench_function("inline_msg", |b| {
        b.iter(|| {
            let mut queue = VecDeque::with_capacity(4_096);
            for generation in 0..4_096 {
                queue.push_back(Msg::Synced { generation });
            }
            let mut sum = 0u64;
            while let Some(Msg::Synced { generation }) = queue.pop_front() {
                sum += generation;
            }
            black_box(sum)
        })
    });
    g.bench_function("boxed_msg", |b| {
        b.iter(|| {
            let mut queue = VecDeque::with_capacity(4_096);
            for generation in 0..4_096 {
                queue.push_back(Box::new(Msg::Synced { generation }));
            }
            let mut sum = 0u64;
            while let Some(msg) = queue.pop_front() {
                if let Msg::Synced { generation } = *msg {
                    sum += generation;
                }
            }
            black_box(sum)
        })
    });
    g.finish();
}

fn wire_json(c: &mut Criterion) {
    use criterion::BatchSize;

    let inputs = [
        ("full", "/tmp/sofka-full-pods.json"),
        ("metadata", "/tmp/sofka-meta-pods.json"),
        ("table", "/tmp/sofka-table-pods.json"),
    ];
    let mut g = c.benchmark_group("plan/wire_json_771");
    for (label, path) in inputs {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        g.throughput(criterion::Throughput::Bytes(bytes.len() as u64));
        g.bench_function(BenchmarkId::new("serde_value", label), |b| {
            b.iter_batched(
                || bytes.clone(),
                |input| black_box(serde_json::from_slice::<Value>(&input).unwrap()),
                BatchSize::LargeInput,
            )
        });
        g.bench_function(BenchmarkId::new("simd_value", label), |b| {
            b.iter_batched(
                || bytes.clone(),
                |mut input| {
                    black_box(
                        simd_json::serde::from_slice::<simd_json::OwnedValue>(&mut input).unwrap(),
                    )
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

#[derive(Default)]
struct VisibleCounter(usize);

impl vte::Perform for VisibleCounter {
    fn print(&mut self, _: char) {
        self.0 += 1;
    }
}

fn manual_visible(bytes: &[u8]) -> usize {
    let mut visible = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() {
                    let byte = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
        } else {
            visible += 1;
            i += 1;
        }
    }
    visible
}

fn ansi_parser(c: &mut Criterion) {
    let input = bs::log_lines(10_000).join("\n").into_bytes();
    let mut g = c.benchmark_group("plan/ansi_10000");
    g.bench_function("specialized_state_machine", |b| {
        b.iter(|| black_box(manual_visible(&input)))
    });
    g.bench_function("vte", |b| {
        b.iter(|| {
            let mut parser = vte::Parser::new();
            let mut performer = VisibleCounter::default();
            parser.advance(&mut performer, &input);
            black_box(performer.0)
        })
    });
    let input_str = std::str::from_utf8(&input).unwrap();
    g.bench_function("ansitok_borrowed", |b| {
        b.iter(|| {
            let visible = ansitok::parse_ansi(input_str)
                .filter(|token| token.kind() == ansitok::ElementKind::Text)
                .map(|token| token.end() - token.start())
                .sum::<usize>();
            black_box(visible)
        })
    });
    g.finish();
}

fn atomic_orderings(c: &mut Criterion) {
    let atom = AtomicU64::new(42);
    let mut g = c.benchmark_group("plan/atomic_1000_loads");
    g.bench_function("seqcst", |b| {
        b.iter(|| {
            let mut sum = 0;
            for _ in 0..1_000 {
                sum += black_box(&atom).load(Ordering::SeqCst);
            }
            black_box(sum)
        })
    });
    g.bench_function("relaxed", |b| {
        b.iter(|| {
            let mut sum = 0;
            for _ in 0..1_000 {
                sum += black_box(&atom).load(Ordering::Relaxed);
            }
            black_box(sum)
        })
    });
    g.finish();
}

fn gzip_and_base64(c: &mut Criterion) {
    let secret = bs::helm_secret(1);
    let wire = secret
        .data
        .pointer("/data/release")
        .unwrap()
        .as_str()
        .unwrap();
    let scalar = base64::engine::general_purpose::STANDARD;
    let simd = base64::engine::Simd::standard(Default::default());

    let mut g = c.benchmark_group("plan/base64_decode");
    for size in [128usize, 1_024, 16_384, wire.len()] {
        let encoded = if size == wire.len() {
            wire.to_owned()
        } else {
            scalar.encode((0..size).map(|i| i as u8).collect::<Vec<_>>())
        };
        g.bench_with_input(BenchmarkId::new("scalar", size), &encoded, |b, encoded| {
            b.iter(|| black_box(scalar.decode(encoded).unwrap()))
        });
        g.bench_with_input(BenchmarkId::new("neon", size), &encoded, |b, encoded| {
            b.iter(|| black_box(simd.decode(encoded).unwrap()))
        });
    }
    g.finish();

    let inner = scalar.decode(wire).unwrap();
    let gzip = scalar.decode(inner).unwrap();
    let isize = u32::from_le_bytes(gzip[gzip.len() - 4..].try_into().unwrap()) as usize;
    let mut g = c.benchmark_group("plan/gzip_output_buffer");
    g.bench_function("empty_vec", |b| {
        b.iter(|| {
            let mut decoder = flate2::read::GzDecoder::new(gzip.as_slice());
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).unwrap();
            black_box(out)
        })
    });
    g.bench_function("isize_capacity", |b| {
        b.iter(|| {
            let mut decoder = flate2::read::GzDecoder::new(gzip.as_slice());
            let mut out = Vec::with_capacity(isize.min(64 * 1024 * 1024));
            decoder.read_to_end(&mut out).unwrap();
            black_box(out)
        })
    });
    g.finish();
}

fn sizes(c: &mut Criterion) {
    eprintln!(
        "plan/size Msg={} bytes",
        std::mem::size_of::<sofka::store::Msg>()
    );
    eprintln!("plan/size String={} bytes", std::mem::size_of::<String>());
    eprintln!(
        "plan/size CompactString={} bytes",
        std::mem::size_of::<compact_str::CompactString>()
    );
    eprintln!(
        "plan/size lasso::Spur={} bytes",
        std::mem::size_of::<lasso::Spur>()
    );
    let mut g = c.benchmark_group("plan/size_guard");
    g.bench_function("msg", |b| {
        b.iter(|| black_box(std::mem::size_of::<sofka::store::Msg>()))
    });
    g.finish();
}

criterion_group!(
    benches,
    render_frame,
    row_identity,
    per_frame_derived_work,
    highlights,
    provider_parse,
    provider_framing,
    pickers_and_headers,
    custom_columns,
    hashers,
    string_representations,
    fuzzy_engines,
    parallel_filter,
    columnar_storage,
    message_queue,
    wire_json,
    ansi_parser,
    atomic_orderings,
    gzip_and_base64,
    sizes,
);
criterion_main!(benches);
