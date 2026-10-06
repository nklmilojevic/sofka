# Performance benchmark: sofka and k9s

This test compared sofka 0.31.0 and k9s 0.51.0 on 6 October 2026.
Both programs used read-only mode.
The results apply to this computer, cluster, configuration, and test method.
They do not establish a general performance advantage.

The [test script](../scripts/benchmark-k9s.py) and
[raw results](benchmarks/2026-10-06.json) are part of this repository.

## Results

Each TUI result uses ten fresh processes per program. The memory result uses
the median of five RSS samples from each process, then the median across the
ten processes.

| Measurement             | sofka median | k9s median |        sofka range |          k9s range |
| ----------------------- | -----------: | ---------: | -----------------: | -----------------: |
| Large pod view          |   2818.31 ms | 3848.97 ms | 2157.78-6444.63 ms | 3757.17-6750.60 ms |
| Apply pod-name filter   |     55.41 ms |  625.51 ms |     40.55-73.08 ms |   602.63-643.27 ms |
| Clear filter            |     33.38 ms |   75.13 ms |     28.52-41.52 ms |     67.37-78.17 ms |
| First StatefulSets view |     80.70 ms |  402.56 ms |    73.42-147.23 ms |   393.97-447.53 ms |
| Process RSS             |   324.41 MiB | 738.23 MiB |  323.41-325.66 MiB |  648.83-751.80 MiB |
| Version command         |      6.72 ms |   43.26 ms |       5.10-9.25 ms |     38.58-46.23 ms |
| Help command            |      6.84 ms |   41.51 ms |       5.30-7.68 ms |     38.95-44.85 ms |

sofka had lower medians for these operations in this test. The programs use
different columns, filter rules, refresh schedules, and API requests. This test
does not isolate the cost of any one implementation choice.

## Environment and build

| Item                | Value                                      |
| ------------------- | ------------------------------------------ |
| Computer            | Apple M3 Max                               |
| Operating system    | macOS, Darwin 27.0.0, arm64                |
| Rust                | rustc 1.98.1 (48a229cea 2026-09-01)        |
| tmux                | tmux 3.7c                                  |
| Terminal            | `xterm-256color`, 180 columns by 50 rows   |
| Pods before / after | 1441 / 1441                                |
| Pod count threshold | 1368                                       |
| StatefulSets before | 5                                          |
| sofka source commit | `ac178b71c11f38d21876e27d96689d4471c46512` |

Build command: `just build-release`. This runs `cargo build --release`.
`Cargo.lock` did not change. sofka uses thin LTO and removes symbols.
k9s came from the installed Nix package. The raw results include executable
hashes and version output. This compares these builds, not matched compiler
or package settings.

| File             |       Bytes |    MiB |
| ---------------- | ----------: | -----: |
| sofka executable |  27,389,952 |  26.12 |
| k9s executable   | 142,893,680 | 136.27 |

The k9s launcher is a 416-byte shell script. Its size is excluded from
the executable size above.
All k9s time results include this launcher. sofka starts directly.
Binary size reflects packaging and included functions as well as implementation.

## Commands and controls

Replace `YOUR_CONTEXT` with the context to test.

```sh
target/release/sofka --context YOUR_CONTEXT --readonly -A pods
k9s --context YOUR_CONTEXT --readonly -A -c pods --splashless --logoless
```

- Both programs used the same kubeconfig and an explicit context. The test did
  not change the current kubeconfig context.
- Both used `--readonly`. The script sent only filter, resource navigation,
  and exit keys. It sent no create, edit, delete, scale, shell, or port-forward command.
- Each TUI process had empty, separate XDG config, data, state, and cache directories.
  The script removed `SOFKA_*` and `K9S_*` environment overrides.
- An isolated tmux server used `/dev/null` as its configuration.
- Ten pairs ran in sequence. sofka ran first in odd pairs; k9s ran first in even pairs.
- The benchmark build and repository checks ran outside the measurement period.
- The operating-system file cache was not cleared. These are new process starts,
  not cold computer starts. The live cluster and other computer activity were not controlled.

## Measurement method

Python `time.perf_counter()` supplied the times. `tmux capture-pane` supplied
visible screen text. The script parsed each program's resource title, including
comma-separated k9s counts. It polled with a 5 ms delay between failed checks.
A screen capture had a median cost of 6.09 ms in 50 samples.
The reported times include input commands, process creation where applicable,
and screen observation delay. Small differences near these costs are not useful
evidence of a product difference.

**Large pod view:** the timer started before tmux replaced the pane process.
It stopped at a visible count of at least 1368 and at least 20 pod status rows.
The threshold was fixed at 95% of the pod count before the test. It does not
mean that all pods or metrics were loaded. The raw results include the count
observed at the end of each start.

**Memory:** the script waited five seconds after the pod threshold, then sampled
`ps -o rss= -p PID` five times at target intervals of 0.25 seconds. macOS reports
RSS in KiB; the script divided by 1,024 for MiB. Samples within a process are
not independent trials. RSS includes shared resident pages and is not private
heap size. Five seconds does not establish a steady state or long-session use.

**Filter:** the script selected one running pod with a unique name before the
test. It sent `/`, the full name, and `Enter`. The timer started before `/` and
stopped when the table count was one and the running pod row was visible.
This includes input before `Enter`, since filtering can start while typing.
The programs use their default filter rules; this is a task comparison, not
a comparison of the same matching algorithm. The clear timer ran from `Escape`
to restoration of the pod threshold.

**StatefulSets:** each process opened this resource for the first time after the
filter test. The timer ran from the initial `:` through `statefulsets` and
`Enter`, until the resource title showed at least 5 objects.
It includes API response time. It does not require complete metrics.

**Version and help:** each command had one warm-up and 100 measured runs.
The script alternated which program ran first. Output went to the null device.
These commands do not connect to Kubernetes. They produce different output and
do not measure TUI response. Commands: `sofka --version`, `k9s version --short`,
`sofka --help`, and `k9s --help`.

The raw data includes p95 values, calculated by linear interpolation at rank
`(n - 1) * 0.95`. Ten process trials give only a limited estimate of variation;
do not treat their p95 values as a reliable tail-latency estimate.

## Scope and excluded attempts

The run completed 20 process trials, with 0 recorded errors. No trial was
excluded. The first trial of each program started slower than the other nine.

This report replaces the 8 September 2026 comparison of sofka 0.24.9. That run
had a sofka startup median of 1514.08 ms on the same cluster. To check whether
the change came from sofka, sofka 0.24.9 was built again and tested on the same
day as this run, three pairs at a time in two alternating rounds. Its startup
median was 2870.5 ms, against 2646.0 ms for sofka 0.31.0 in the same rounds.
The slower startup came from the test environment, not from sofka. Do not use
the two reports to claim a performance improvement or regression.

This test did not measure CPU under a fixed event rate, API request counts,
network bytes, garbage collection pauses, long sessions, or other operating
systems. It did not replay a fixed object set. Row movement was omitted because
background screen changes can trigger a false result. Raw status-text matching
was omitted because it does not establish that a row is visible.

## Run the test again

```sh
just build-release
python3 scripts/benchmark-k9s.py --context YOUR_CONTEXT --pairs 10 \
  --output docs/benchmarks/NEW-RUN.json
```

Use `--sofka` and `--k9s` to select other builds. The script requires an existing
cluster with at least 22 pods and one StatefulSet. It stores timings, counts,
and build metadata. It does not store pod names or screen captures.
Each new run uses the selected cluster's current objects; it is not an exact replay.
