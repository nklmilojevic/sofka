use super::*;
use crate::columns::usage_pct;
use std::time::Instant;

const SAMPLES: usize = 60;
const BIN_SECONDS: u64 = 5;
const NODE_BINS: u64 = 12;
const NODE_BIN_SECONDS: u64 = 25;
const TREND_LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TrendTarget {
    generation: u64,
    key: String,
    uid: Option<String>,
}

#[derive(Default)]
pub(crate) struct ContainerHistory {
    target: Option<TrendTarget>,
    samples: VecDeque<(Instant, Option<(i64, i64)>)>,
}

impl ContainerHistory {
    fn select(&mut self, target: Option<TrendTarget>) {
        if self.target != target {
            self.target = target;
            self.samples.clear();
        }
    }

    fn record(&mut self, value: Option<(i64, i64)>, now: Instant) {
        if self.target.is_none() {
            return;
        }
        while self.samples.front().is_some_and(|(time, _)| {
            now.saturating_duration_since(*time).as_secs() >= SAMPLES as u64 * BIN_SECONDS
        }) {
            self.samples.pop_front();
        }
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now, value));
    }

    fn bars(&self, now: Instant, cpu: bool) -> [Option<u64>; SAMPLES] {
        let mut bars = [None; SAMPLES];
        for (time, value) in &self.samples {
            let age = now.saturating_duration_since(*time).as_secs() / BIN_SECONDS;
            if age < SAMPLES as u64 {
                bars[SAMPLES - 1 - age as usize] = value
                    .map(|(c, m)| if cpu { c } else { m })
                    .and_then(|v| u64::try_from(v).ok());
            }
        }
        bars
    }
}

/// The node's UID and its peak (cpu, memory) per bin, oldest first.
type NodeBins = (Option<String>, VecDeque<(u64, (i64, i64))>);

/// Node usage history for the trend columns: the peak (cpu, memory) sample
/// per 25-second bin, counted from the first sample so bins never shift.
#[derive(Default)]
pub(crate) struct NodeHistory {
    generation: u64,
    start: Option<Instant>,
    pub(super) nodes: HashMap<String, NodeBins>,
}

impl NodeHistory {
    fn bin(&self, now: Instant) -> Option<u64> {
        Some(now.saturating_duration_since(self.start?).as_secs() / NODE_BIN_SECONDS)
    }

    fn record(
        &mut self,
        generation: u64,
        data: impl IntoIterator<Item = (String, Option<String>, (i64, i64))>,
        now: Instant,
    ) {
        if self.generation != generation {
            *self = Self {
                generation,
                ..Self::default()
            };
        }
        let bin = self.bin(now).unwrap_or_else(|| {
            self.start = Some(now);
            0
        });
        for (name, uid, (cpu, mem)) in data {
            let (known, bins) = self.nodes.entry(name).or_default();
            // A node recreated under the same name starts a new history.
            if *known != uid {
                *known = uid;
                bins.clear();
            }
            match bins.back_mut() {
                Some((last, peak)) if *last == bin => {
                    *peak = (peak.0.max(cpu), peak.1.max(mem));
                }
                _ => bins.push_back((bin, (cpu, mem))),
            }
        }
        let oldest = (bin + 1).saturating_sub(NODE_BINS);
        self.nodes.retain(|_, (_, bins)| {
            while bins.front().is_some_and(|(b, _)| *b < oldest) {
                bins.pop_front();
            }
            !bins.is_empty()
        });
    }

    fn cell(
        &self,
        name: &str,
        uid: Option<&str>,
        allocatable: Option<i64>,
        cpu: bool,
        now: Instant,
    ) -> String {
        let (Some(bin), Some((known, bins))) = (self.bin(now), self.nodes.get(name)) else {
            return "·".repeat(NODE_BINS as usize);
        };
        if known.as_deref() != uid {
            return "·".repeat(NODE_BINS as usize);
        }
        (0..NODE_BINS)
            .map(|i| {
                let wanted = (bin + i + 1).checked_sub(NODE_BINS);
                bins.iter()
                    .find(|(b, _)| Some(*b) == wanted)
                    .and_then(|(_, (c, m))| usage_pct(if cpu { *c } else { *m }, allocatable))
                    .map_or('·', |pct| {
                        TREND_LEVELS[((pct.clamp(0, 100) * 8 + 99) / 100) as usize]
                    })
            })
            .collect()
    }
}

impl App {
    fn container_trend_target(&self) -> Option<TrendTarget> {
        if self.mode != Mode::Containers
            && !(self.mode == Mode::Command && self.palette_return == Mode::Containers)
        {
            return None;
        }
        let (ns, pod) = self.container_pod.as_ref()?;
        let container = self.container_list.get(self.container_state.selected()?)?;
        let pod_key = format!("{ns}/{pod}");
        let obj = self.store.get(&pod_key)?;
        Some(TrendTarget {
            generation: self.generation,
            key: format!("{pod_key}/{container}"),
            uid: obj.metadata.uid.clone(),
        })
    }

    pub(super) fn sync_container_history(&mut self) {
        self.container_history.select(self.container_trend_target());
    }

    pub(super) fn record_container_history(&mut self, failed: bool) {
        self.sync_container_history();
        let value = self.container_history.target.as_ref().and_then(|target| {
            if failed {
                None
            } else {
                self.container_metrics.get(&target.key).copied()
            }
        });
        self.container_history.record(value, Instant::now());
    }

    pub(crate) fn container_trend_bars(&self, cpu: bool) -> [Option<u64>; SAMPLES] {
        if self.container_history.target != self.container_trend_target() {
            return [None; SAMPLES];
        }
        self.container_history.bars(Instant::now(), cpu)
    }

    pub(super) fn record_node_history(&mut self) {
        if self.kind_plural == "nodes" {
            let samples = self.metrics.iter().map(|(name, usage)| {
                let uid = self.store.get(name).and_then(|o| o.metadata.uid.clone());
                (name.clone(), uid, *usage)
            });
            self.node_history
                .record(self.generation, samples, Instant::now());
        }
    }

    pub(crate) fn node_trend_cell(&self, obj: &DynamicObject, cpu: bool) -> String {
        let (cpu_alloc, mem_alloc) = crate::columns::node_allocatable(obj);
        let name = obj.metadata.name.as_deref().unwrap_or_default();
        let allocatable = if cpu { cpu_alloc } else { mem_alloc };
        if self.node_history.generation != self.generation {
            return "·".repeat(NODE_BINS as usize);
        }
        self.node_history.cell(
            name,
            obj.metadata.uid.as_deref(),
            allocatable,
            cpu,
            Instant::now(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_bounded_and_missing_values_are_not_zero() {
        let mut history = ContainerHistory::default();
        history.select(Some(TrendTarget {
            generation: 1,
            key: "ns/pod/app".into(),
            uid: Some("a".into()),
        }));
        let start = Instant::now();
        for i in 0..100 {
            history.record(Some((i, i * 2)), start + Duration::from_secs(i as u64 * 5));
        }
        assert_eq!(history.samples.len(), 60);
        let now = start + Duration::from_secs(500);
        history.record(None, now);
        let bars = history.bars(now, true);
        assert_eq!(bars[59], None);
        assert_eq!(bars[58], Some(99));
        history.record(Some((0, 0)), now + Duration::from_secs(5));
        assert_eq!(
            history.bars(now + Duration::from_secs(5), true)[59],
            Some(0)
        );
        assert_eq!(
            history.bars(now + Duration::from_secs(400), true),
            [None; 60]
        );
        history.select(Some(TrendTarget {
            generation: 1,
            key: "ns/pod/app".into(),
            uid: Some("b".into()),
        }));
        assert!(history.samples.is_empty());
    }

    #[test]
    fn node_bins_keep_peaks_mark_gaps_and_expire() {
        let mut history = NodeHistory::default();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let sample = |cpu| [("node".to_string(), Some("a".to_string()), (cpu, 0))];
        history.record(1, sample(500), at(0));
        history.record(1, sample(1000), at(5));
        history.record(1, sample(0), at(50));
        assert_eq!(
            history.cell("node", Some("a"), Some(1000), true, at(50)),
            "·········█· "
        );
        assert_eq!(
            history.cell("node", Some("a"), None, true, at(50)),
            "·".repeat(12)
        );
        assert_eq!(
            history.cell("other", Some("a"), Some(1000), true, at(50)),
            "·".repeat(12)
        );
        assert_eq!(
            history.cell("node", Some("b"), Some(1000), true, at(50)),
            "·".repeat(12)
        );
        history.record(
            1,
            [("node".to_string(), Some("b".to_string()), (1000, 0))],
            at(55),
        );
        assert_eq!(
            history.cell("node", Some("b"), Some(1000), true, at(55)),
            format!("{}█", "·".repeat(11))
        );
        history.record(1, sample(10), at(300));
        assert_eq!(
            history.cell("node", Some("a"), Some(1000), true, at(300)),
            "···········▁"
        );
        history.record(1, [], at(600));
        assert!(history.nodes.is_empty());
        history.record(2, sample(1000), at(600));
        assert_eq!(history.generation, 2);
        assert_eq!(
            history.cell("node", Some("a"), Some(1000), true, at(600)),
            "···········█"
        );
    }
}
