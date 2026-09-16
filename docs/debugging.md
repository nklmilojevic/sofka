# Debugging and incident workflow

## Explain unhealthy (`X`)

A deterministic, evidence-based answer to "why is this broken?" for the selected
object: rollout state, degraded conditions, the blocking pods and their container
failure reasons (ImagePullBackOff, CrashLoopBackOff, OOMKilled, unschedulable,
failed probes), and recent Warning events. No AI, no external service.

`j`/`k` move, `⏎` goes to the resource behind a finding, `E` its events, `l` its
logs, `r` gathers again. A finding you can drill into has a trailing `→`.

For Nodes, memory, disk, and PID pressure are warnings when their conditions
are `True`. `NetworkUnavailable=True` is also a warning. These conditions do
not produce warnings when they are `False`. `Unknown` remains a warning, and
the `Ready` condition is assessed separately.

The DaemonSet rollout summary reads available pods from `status.numberAvailable`.

## Timeline (`T`)

A per-object timestamped log of every state change the watch saw this session:
generation bumps, replica and readiness changes, pod phase, restarts, waiting
reasons, condition flips. Diffed from the watch stream, bounded in size, never
written to disk.

Restart history includes normal init containers and native sidecars. It keeps
completed init restart counts in its total, so the end of initialization does
not reset the count. The pod table excludes normal init restarts after
initialization is complete.

## Diff (`:diff`)

A unified diff of the live object against its `last-applied-configuration`. When
that annotation is missing - as it is for every Flux-, ArgoCD-, or Helm-managed object,
which nothing ever `kubectl apply`s - sofka diffs against the previous revision
this session's watch saw instead, so "what just changed?" has an answer on GitOps
clusters. The last revision of up to 256 changed objects is kept in memory.

## Notifications

`:notify` toggles a notification on the selected object. Sophie watches it so you
don't have to: every state change the watch sees (the same transitions the
timeline records - rollout progress, readiness, phase, restarts, waiting reasons,
conditions) flashes in the status line, rings the terminal bell, and fires a
**desktop notification**.

Each notify is its own bounded single-object watch, so it keeps firing while you
browse other views - "tell me when this rollout finishes" and keep working.
`:notify` on the same row turns it off. Switching contexts stops all active
notifications so events from the previous cluster cannot cross contexts.
Everything is session-local.

```toml
[notify]
bell = true         # ring the terminal bell
desktop = "osc777"  # "osc777" | "osc9" | "both" | "off"
# command = ["notify-send", "sofka", "$MESSAGE"]     # Linux, inside tmux
# command = ["terminal-notifier", "-title", "sofka"] # macOS ($MESSAGE appended)
```

- `osc777` (default) - rxvt-style title+body, the form Ghostty recommends. Also
  kitty, WezTerm, foot, urxvt.
- `osc9` - iTerm2-style body-only, for iTerm2 and Windows Terminal, which speak
  only that.
- `both` and `off` are also valid. Terminals ignore protocols they don't speak.

Inside a **terminal multiplexer**, which swallows escape sequences from its panes,
set `command` to run a local notifier subprocess instead (`$MESSAGE` is
substituted as a whole argument, never through a shell).

In a **herdr** pane no config is needed at all: sofka detects the pane
environment and delivers through `herdr notification show`, so the toast follows
herdr's own `ui.toast` delivery (in-app, outer terminal, or system).

## Log controls

In the pod table, use `Space` to mark pods, then press `l` to open their combined
logs. This includes pods from different namespaces. Each line has a
`[namespace/pod:container]` prefix. Only marked pods still present in the filtered
table are included. With no marks, `l` opens logs for the current row.

The pod set is fixed when the view opens, including when timestamps or time
anchors change, or streaming resumes. New pods are not added automatically.
Lines with timestamps are sorted by time, even when timestamp text is hidden.
Press `t` to show or hide timestamps without clearing the buffer or restarting
the streams. A paused view keeps the same log line or marker in view, within
the scroll limits. Lines with equal timestamps keep their arrival order. Lines
without a valid timestamp use the newest known time for sorting. If no time is
known, sofka uses the arrival time. A source
error includes its prefix, and other streams continue. The existing filter and
buffer controls apply to the combined view. `p` and `L` still use the current row.

The kubelet logs view (`l`) keeps a bounded follow buffer. Tune the initial tail,
the buffer size, and an optional `since` lookback:

```toml
[logs]
tail = 300         # initial lines fetched per stream (kubectl --tail)
buffer = 5000      # max lines kept while following (oldest dropped)
since = "1h"       # optional: only logs newer than this, within the tail limit
fullscreen = false # open log views fullscreen (F toggles per session)
```

The `since` window and the `1`–`5` time anchors keep the initial line limit.
A pod stream requests at most `tail` initial lines per container. Workload and
Service streams request at most `min(tail, 100)` initial lines per container.
The time window can reduce this number. Live following continues after these
initial lines. Previous-container logs keep their full history.

In the view, `/` filters with a case-insensitive substring, a `/regex/`, or a
leading `!` to invert (keep lines that don't match). A malformed regex is flagged
instead of hiding everything. `z` clears the on-screen buffer while the live
stream keeps appending. A pod streams every container's logs at once. Full keymap:
[Logs view](keys.md#logs-view).

For history that outlives the pod, use [VictoriaLogs](providers.md#log-provider-victorialogs).

## Debug containers and pods

`:debug` on a **pod** attaches a temporary ephemeral debug container with
`kubectl debug`. sofka prompts for the image (prefilled from `[debug]`). An empty
`command` starts an interactive shell (bash if the image has it, else sh), like
the pod shell. `d` in the container picker sets `--target=<container>` so the
debug container shares that container's process namespace. The ephemeral
container stays on the pod until the pod is recreated - Kubernetes can't remove
it, so there's nothing for sofka to clean up.

`:debug` on a **node** starts a privileged diagnostic pod on it
(`kubectl debug node/<node>`, image `node_image` in `node_namespace`, optional
`node_profile`). That pod mounts the host filesystem at `/host` and joins the host
PID, network, and IPC namespaces, so sofka previews exactly that access and makes
you confirm before creating it. sofka records the node debuggers it started this
session and `:debug-clean` deletes them (matched by the `node-debugger-*` name and
the node). kubectl leaves the pod behind after you exit, so clean up when you're
done.

```toml
[debug]
image = "nicolaka/netshoot:latest"       # ephemeral (in-pod) debug image
command = ["bash"]                       # entrypoint; omit for an interactive shell
node_image = "nicolaka/netshoot:latest"  # node debug pod image
node_namespace = "default"               # namespace the node debugger lands in
node_profile = "sysadmin"                # kubectl debug --profile (optional)
```

Read-only mode and [guardrails](safety.md#guardrails) gate both actions: the
`debug` action for pods, `node-debug` for nodes. Both are recorded in the
[journal](safety.md#action-journal).

## Diagnostic bundles

`:bundle` assembles a redacted incident bundle for the selected object - its YAML,
the owner, the incident explanation, recent events, the session timeline, bounded
recent logs, and a metrics snapshot - into one Markdown document. It's for handing
an incident between application and platform teams. sofka gathers it off-thread
and shows a preview, then `:bundle-save` writes it to a temp file.

Always redacted: Secret `data`/`stringData` values, any credential-like
annotation (a key containing `token`, `password`, `secret`, `apikey`,
`credential`, and similar), and `last-applied-configuration`, all replaced with a
placeholder. `managedFields` is dropped. Env vars sourced from Secrets are flagged
(their values are references, not literals). Every bundle carries a manifest of
exactly what it includes and what it withholds.

```toml
[bundle]
anonymize = false   # replace context/cluster identity with placeholders
log_lines = 200     # max recent log lines per pod
max_pods = 3        # cap how many pods contribute logs
```

## Snapshots

`:snapshot` captures the current table view - its columns and visible rows, plus
metadata (context, cluster, namespace, resource, filter, timestamp) - to a file.
An optional argument sets the format: `text` (default, an aligned table with a
header block), `json`, or `yaml`. Files land in
`$XDG_STATE_HOME/sofka/snapshots` (or `~/.local/state/sofka/snapshots`).

`:snapshots` browses saved captures, newest first with their age. `⏎` opens one in
a viewer with a staleness banner (it's a point-in-time capture), `d` deletes the
highlighted file.

This is not the one-frame `--snapshot` CI flag - this is an interactive
capture-and-review workflow.

## Runtime diagnostics

`sofka info`, structured logging, and credential and IP address redaction are
available from sofka 0.24.8. Run `sofka --version` and update older versions
before collecting diagnostics for a bug report.

`:info` shows the version and build, config sources, live context/cluster/API
server and Kubernetes revision, discovery and Metrics API status, watch error
and reconnect counts, API request latency, the logging destination, and the
state/log/snapshot/bundle directories. It also names the active skin and the
plugins and custom views that loaded.

`sofka info` prints the same report headlessly. It connects briefly - discovery
and Metrics API status are the half of the report that is not on disk - and
still prints everything else if the connection fails:

```sh
sofka info              # connect, report, exit
sofka info --offline    # no connection: build, config, logging, directories
```

`:info` reports the running session's watch error and reconnect counts. A
headless report has no session to count, so it opens one watch instead - the
same resource and namespace a launch would - and reports whether it establishes,
how long the initial sync and watch connection took, and how many objects the
initial list returned. A successful list alone does not count as an established
watch. The probe also waits for successful watch response headers. Discovery
working says nothing about whether watches do: a proxy that closes long-lived
connections passes every other check in this report and still leaves the TUI
with an empty table. The probe gives up after 5s, which is itself the answer
when a first view is too slow to be usable.

Identifiers, paths, and counts only, never credentials, tokens, decoded Secret
values, or plugin inputs. Values that could carry a credential - an API server
URL with userinfo, an error string echoing a request header - are redacted
before they are printed. Literal IPv4 and IPv6 addresses are also masked in
reports and logs. URL schemes, ports, and paths remain available for diagnosis.
The `:info` screen also masks its header and status line. Hostnames, resource
names, and file paths can still identify your setup; check them before sharing.

### Request latency

Every Kubernetes API request is timed and bucketed by class, so "the cluster
feels slow" becomes a number:

```
API request latency
  CLASS        COUNT  ERRORS       AVG       P50       P90       MAX
  discovery        4       0     182ms     262ms     524ms     341ms
  watch           12       0      41.2ms    65.5ms   131ms     118ms
  read            37       1      12.8ms    16.4ms    32.8ms    91.2ms
```

`watch` is time to response headers - the stream itself stays open for the life
of the view. Percentiles are bucket upper bounds (powers of two), so read them
as "at most"; `AVG` and `MAX` are exact. `ERRORS` includes requests canceled
before response headers (for example by a timeout), transport failures, and 5xx
responses. A 4xx response is not counted as a latency error.

### Structured logging

Off by default. Turn it on for one run with `SOFKA_LOG`, or in config:

```toml
[logging]
level       = "info"   # off (default) | error | warn | info | debug | trace
# file      = "/tmp/sofka.log"   # default: <state-dir>/logs/sofka.log
max_size_mb = 8        # rotate to <file>.1 past this size
```

```sh
SOFKA_LOG=debug sofka pods    # overrides [logging] level for one run
tail -f ~/.local/state/sofka/logs/sofka.log
```

Each event is one logfmt line:

```
ts=2026-09-06T18:54:31.845Z level=info event=cluster.connected context=prod cluster=eu-1 kinds=214
ts=2026-09-06T18:54:32.001Z level=info event=watch.start kind=pods ns=default generation=1
ts=2026-09-06T18:54:38.774Z level=warn event=watch.error kind=pods error="too old resource version"
```

`info` covers the session shape - startup, connects, watch starts and re-lists.
`debug` adds one line per API request. `warn` and `error` carry watch failures,
failed requests, state-write failures, and background-task panics.

Every value is redacted on the way in - bearer tokens, kubeconfig credentials,
`key=value` pairs whose key looks like a credential, and URL userinfo - so the
log can be attached to a bug report as it is. Writing happens on its own thread
behind a bounded queue: a stalled filesystem drops lines (counted in `:info`)
rather than stalling the UI.

Concurrent sessions can use the same log file. Writer threads use `<file>.lock`
to coordinate writes and rotation. Each writer opens the current file and reads
its size while it holds the lock. Keep the lock file in place while sessions run.

If a kind is missing, check the discovery line first. When sofka cannot read an
API group, it shows a warning at startup:
`warning: API discovery could not read <group>/<version>: <reason>`. `:info`
shows the same warnings under the cluster discovery status. `sofka --check`
also prints the warnings and the number of API groups that sofka did not read.
Sofka cannot skip the core API group. If it cannot read `v1`, the connection
fails with the reason. If aggregated discovery fails, sofka shows the reason
and reads each API group separately.

## TLS session resumption and HTTP 401

Some endpoints, including the AKS endpoints reported in
[issue #479](https://github.com/nklmilojevic/sofka/issues/479), return HTTP 401
on resumed TLS connections when authentication uses a client certificate.
Sofka can show `watch stream failed: ApiError: Unauthorized` at startup or
when you change context. Other requests, including metrics requests, can fail too.

For an affected cluster, disable TLS session resumption for this run:

```sh
sofka --no-tls-resumption --context my-aks
sofka --no-tls-resumption --context my-aks --check
sofka --no-tls-resumption --context my-aks --snapshot -A pods
```

This option is off by default. When selected, it applies to all contexts used
in the run, including fleet queries and the bundled sanitizer. Each new cluster
TLS connection uses a full handshake. Existing connections can still be reused.
Server certificate and hostname checks remain enabled. This option is separate
from `--allow-v1-client-cert` and does not permit v1 certificates by itself.
Full handshakes add work on new connections, including later reconnects.

A 401 alone does not identify this problem. Expired or missing credentials can
also cause it. To check session resumption, use OpenSSL with the certificate,
key, and CA for the affected context. Use the API hostname for `HOST`, or its
`tls-server-name` override, and the API address with its port for `ADDR`:

```sh
HOST=api.example.com
ADDR=api.example.com:443
umask 077
printf 'GET /version HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n' "$HOST" > req.txt
openssl s_client -tls1_3 -connect "$ADDR" -servername "$HOST" \
  -cert tls.crt -key tls.key -CAfile ca.crt \
  -sess_out sess.pem -ign_eof < req.txt
openssl s_client -tls1_3 -connect "$ADDR" -servername "$HOST" \
  -cert tls.crt -key tls.key -CAfile ca.crt \
  -sess_in sess.pem -ign_eof < req.txt
```

Compare `New` or `Reused` with the HTTP status. Full handshakes that return 200
and resumed handshakes that return 401 support this diagnosis. A server can
reject a ticket and use a full handshake, so more than one attempt can be needed.
Keep private keys and session files private. Remove the temporary files after
the check.

Watch errors use increasing retry delays to limit repeated requests. This delay
is always enabled and does not correct an authentication failure.

## X.509 v1 client certificates

Some MicroK8s kubeconfigs contain an X.509 v1 client certificate. The standard
rustls client certificate loader rejects this format. Sofka identifies this
failure as a client certificate error and rejects the connection by default.

To allow this format for one run, pass the explicit flag:

```sh
sofka --allow-v1-client-cert
sofka --allow-v1-client-cert --check
sofka --allow-v1-client-cert --context microk8s
```

The flag applies to context switches, fleet connections, and bundled plugin
adapters in that run. It is not saved to configuration and does not change
kubeconfig. It supports static `client-certificate-data` / `client-key-data`
and `client-certificate` / `client-key` files. It does not support v1
certificates returned by exec credential plugins. Exec plugins with supported
certificates continue to use the standard client path.

Sofka checks that the v1 certificate matches its private key. The flag does not
disable server certificate verification or change TLS versions and ciphers.
Existing kubeconfig trust settings still apply. X.509 v1 is a certificate
format, not TLS 1.0. V1 certificates cannot contain usage restrictions such as
an extended key usage for client authentication. The API server still decides
whether to accept the client certificate.

To check an inline client certificate for the selected context:

```sh
kubectl config view --raw --minify -o jsonpath='{.users[0].user.client-certificate-data}' \
  | openssl base64 -d -A | openssl x509 -noout -text
```

For a certificate file, use `openssl x509 -in client.crt -noout -text`.
`Version: 1 (0x0)` identifies a v1 certificate. To remove the need for the
flag, have the cluster administrator issue a v3 client certificate and update
your kubeconfig. Keep the existing identity and required permissions.

## HTTPS proxy exclusions

If the API server must use a direct connection, include its host name or IP
address in `NO_PROXY` or `no_proxy`. For example:

```sh
NO_PROXY=rke2-server sofka --check
```

The first value with non-whitespace text takes priority: `NO_PROXY`, then
`no_proxy`. Values that contain only whitespace are ignored. Separate
entries with commas. A domain such as `example.com` matches that domain and its
subdomains. A leading dot or `*.` matches subdomains only. Entries can also be IP
addresses, CIDR ranges, or host names and IP addresses with a port. Use brackets
for an IPv6 address with a port, such as `[2001:db8::1]:6443`. A single `*` excludes
all servers. CIDR entries match literal server IP addresses; sofka does not
resolve host names to test these entries.

These exclusions apply to the environment proxy on startup and context changes.
Servers without a matching exclusion keep using the proxy. An explicit
`proxy-url` in the selected kubeconfig cluster takes priority over exclusions.

To test whether the environment proxy causes a connection failure, remove its
settings for one run:

```sh
env -u HTTPS_PROXY -u https_proxy sofka --check
```

## Teleport local Kubernetes proxy certificates

`tsh proxy kube` can serve a CA certificate as its server certificate. Some TLS
clients reject this with `CaUsedAsEndEntity`, even when the kubeconfig trusts
that exact certificate.

Sofka accepts this setup when the server certificate exactly matches a CA
certificate in the selected kubeconfig's `certificate-authority` file or
`certificate-authority-data`. No extra flag is required. This also applies when
you change contexts or use fleet mode.

Sofka still checks the certificate dates, hostname (including `tls-server-name`),
allowed usage, and TLS signatures. A different certificate with the same key or
subject does not qualify for this exception. Certificates with name constraints
or unsupported critical extensions do not qualify either. Other server
certificates use standard verification. System trust and in-cluster CA file
reloads continue to use the kube client verifier.

The exception also applies to `auth-provider` entries with `name: oidc`.
Kubeconfigs with exec credential plugins or other `auth-provider` entries keep
standard verification. These providers can run external commands, so they keep
the standard client construction path.

The `--allow-v1-client-cert` flag is separate. It controls the format of the
client certificate and is not needed for a Teleport CA server certificate.

## OIDC token refresh

Sofka supports token refresh for kubeconfig `auth-provider` entries with
`name: oidc`. It uses the cached `id-token` while it is valid and attempts to
refresh it near expiry. The provider configuration must include an `id-token`.
Refresh also requires `idp-issuer-url`, `client-id`, `client-secret`, and
`refresh-token`. These requirements come from kube-client 4.2.0.

The identity provider uses a separate HTTPS connection with system trust.
The Kubernetes API server CA exception does not apply to that connection.
