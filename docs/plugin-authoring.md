# Create a plugin package

A plugin package adds an external tool to sofka.
The package contains a manifest and, if necessary, an adapter.
An adapter is a program that changes tool output into a sofka report.
The plugin protocol supports adapters in any programming language.

sofka supplies resource data, process control, output limits, and report display.
The package supplies tool commands and result interpretation.
A new package does not require changes to sofka's Rust code.

## Language choice

For core plugins maintained and shipped by the sofka project, prefer Rust.
This keeps the implementation consistent with sofka and reduces the need for extra language runtimes.
Rust is a preference, not a requirement of the plugin protocol.

External plugins can use any programming language.
They must follow the same manifest, request, report, and execution rules.

The Python example below shows how to use the protocol.
It does not set the language preference for core plugins.

## Terms

| Term     | Meaning                                                                 |
| -------- | ----------------------------------------------------------------------- |
| Package  | A directory that contains `plugin.toml` and related files.              |
| Manifest | The `plugin.toml` file that describes a plugin.                         |
| Adapter  | The program that sofka starts for a plugin.                             |
| Request  | The JSON object that sofka sends to the adapter through standard input. |
| Report   | The JSON object that the adapter writes to standard output.             |
| Input    | A named value that the user supplies with a plugin command.             |

## Create your first package

The example requires Python 3.
It reads the selected object without a cluster request.

1. Copy `examples/plugins/resource-summary/` into your sofka configuration directory.

   ```sh
   mkdir -p ~/.config/sofka/plugins
   cp -R examples/plugins/resource-summary ~/.config/sofka/plugins/
   ```

   If you set `XDG_CONFIG_HOME`, use `$XDG_CONFIG_HOME/sofka/plugins/` instead.

2. Check the package.

   ```sh
   sofka --validate-plugin ~/.config/sofka/plugins/resource-summary
   ```

3. In sofka, enter `:reload`.
4. Select a resource.
5. Enter `:resource-summary detail=true`.

The report opens in the document view.
Press `/` to search the report.
Press `esc` to return to the resource view.

## Package files

```text
plugins/
  resource-summary/
    plugin.toml
    adapter.py
    request.json
```

sofka reads `plugins/*/plugin.toml` at start, on `:reload`, and on a context change.
It does not run adapters during package discovery.
It checks package directories in name order.
An invalid package does not stop other packages.
The `:config` view shows package errors.
Package manifests reject unknown fields, including fields in input definitions.
Inline plugin entries and their input definitions ignore unknown fields to preserve existing configuration.

An inline `[[plugins]]` entry has priority over a package with the same name or command.
The first package has priority over a later duplicate.
Built-in commands and resource names have priority over plugin commands.
To remove a package, remove its directory.
Then enter `:reload`.

## Manifest

```toml
schema_version = 1

[plugin]
name = "Resource summary"
palette = "resource-summary"
command = "python3"
args = ["adapter.py"]
requires = ["python3"]
install = "Install Python 3 to run this example."
output = "report"
mutating = false
timeout = "10s"

[plugin.inputs.detail]
type = "boolean"
default = "false"
```

| Field            | Function                                                                                   |
| ---------------- | ------------------------------------------------------------------------------------------ |
| `schema_version` | Required package format version. Use `1`.                                                  |
| `name`           | Required display name.                                                                     |
| `palette`        | Optional command name, without `:`. Use lowercase letters, digits, or hyphens.             |
| `key`            | Optional key chord. Supply `key`, `palette`, or both.                                      |
| `command`        | Required executable name or path.                                                          |
| `args`           | Command arguments. Each item is one argument.                                              |
| `requires`       | Executables that must be available. sofka also checks `command`.                           |
| `install`        | Instructions that appear when an executable is absent.                                     |
| `scopes`         | Resource plurals where the plugin can run. An empty list permits all resource kinds.       |
| `target`         | `selection` by default. Use `context` to run once without a selected object.               |
| `output`         | Required package output mode: `popup`, `background`, or `report`.                          |
| `timeout`        | Maximum time for each target. Default: `30s`.                                              |
| `mutating`       | `true` by default. Use `false` only when the plugin does not change cluster resources.     |
| `network_load`   | Set to `true` when the plugin generates test traffic. Default: `false`.                    |
| `confirm`        | Require confirmation before execution. Default: `false`.                                   |
| `dangerous`      | Require confirmation and identify the action as dangerous. Default: `false`.               |
| `port_forward`   | Optional remote port for the selected pod or service. See [Port-forwards](#port-forwards). |
| `inputs`         | Input definitions. See [Inputs](#inputs).                                                  |

The adapter's working directory is the package directory.
Use `command = "./adapter"` for an executable in that directory.
The executable must stay within the package directory.
Arguments can refer to other package files, such as `adapter.py`.

Packages do not permit `shell = true` or `output = "terminal"`.
Existing inline plugins continue to support these modes.
A package can contain an executable shell script.
Use separate arguments when that script starts another program.

If an executable is absent, the command remains in the command palette.
sofka shows the absent executable and the `install` text when the user selects the command.
It checks executables again before each run.

## Inputs

Supply inputs as `name=value` arguments.
Separate arguments with spaces.
Input values cannot contain spaces in this version.

```text
:benchmark duration=10s connections=20 port=8080
```

```toml
[plugin.inputs.duration]
type = "duration"
default = "10s"
min = 1
max = 300

[plugin.inputs.connections]
type = "integer"
default = "20"
min = 1
max = 1000

[plugin.inputs.port]
type = "integer"
min = 1
max = 65535
```

The `port` input has no default.
The user must supply it before the adapter can start.
A key chord uses the default input values.

| Type       | Permitted values                         |
| ---------- | ---------------------------------------- |
| `string`   | Text without spaces.                     |
| `integer`  | An unsigned integer.                     |
| `boolean`  | `true` or `false`.                       |
| `duration` | A duration such as `10s`, `2m`, or `1h`. |

Write default values as TOML strings.
Use `min` and `max` only with integers or durations.
Duration limits use seconds.
Use `choices = ["summary", "full"]` to restrict an input to specified values.

sofka rejects unknown inputs, duplicate inputs, invalid values, and absent required inputs before execution.
The request contains the validated input values as strings.

To pass an input through an argument, use a complete placeholder:

```toml
args = ["adapter.py", "--duration", "${input.duration}"]
```

sofka preserves each input as one argument.
It does not interpret input values as shell commands or other placeholders.
Existing resource placeholders, such as `$NAME` and `$NAMESPACE`, are also available in `args`.
Use the JSON request when you need complete resource data.

## Request format

sofka writes one JSON object to standard input.
It then closes standard input.
The adapter must read the request before it writes its report.

```json
{
  "schema_version": 1,
  "context": "development",
  "kubeconfig": null,
  "cluster": "development",
  "namespace": "default",
  "resource": "pods",
  "name": "api",
  "filter": "",
  "inputs": { "detail": "true" },
  "object": {
    "apiVersion": "v1",
    "kind": "Pod",
    "metadata": { "name": "api", "namespace": "default" }
  },
  "forward": null
}
```

The adapter inherits sofka's environment, including `KUBECONFIG`.
The `context` value is `null` when sofka has no explicit kubeconfig context name.
In that case, let the external tool use its configuration.
Do not pass a synthetic context name to the tool.
The `kubeconfig` value names the file that context lives in, and is `null` when
that file is the one the environment already resolves.
When it is set, pass it to the tool as well: the context does not exist in the
kubeconfig the tool would read on its own.

For `target = "selection"`, sofka starts one job for each marked resource.
If no resources are marked, it uses the selected resource.
The request contains that resource's namespace and object data.

For `target = "context"`, sofka starts one job.
The `object` value is `null`, and `name` is empty.
The `namespace` value is the current namespace.
An empty namespace means all namespaces.
The adapter must translate this value into the tool's namespace arguments.

## Report format

With `output = "report"`, the adapter writes one JSON report to standard output.
Use standard error for diagnostic messages.
Return exit code `0` for a complete report.
Return a nonzero exit code for an execution error.

```json
{
  "schema_version": 1,
  "title": "Scan results",
  "sections": [
    {
      "title": "Summary",
      "lines": ["One resource requires attention."]
    },
    {
      "title": "Findings",
      "columns": ["Resource", "Severity", "Description"],
      "rows": [["api", "high", "Example finding"]]
    }
  ]
}
```

Each section requires a title.
The `lines`, `columns`, and `rows` fields are optional.
All text and table cells must be strings.
Each row must have the same number of cells as the column list.

sofka shows the sections and tables in one searchable document.
This version does not support row actions or table sorting.
It replaces control characters in report text with spaces.
An invalid report produces a visible error.

Some tools return nonzero exit codes when they find problems.
The adapter must distinguish findings from execution errors.
For valid findings, write the report and return `0`.

## Execution limits

sofka runs a maximum of eight target jobs at the same time.
Each job has the configured timeout.
The timeout includes port-forward setup, request transfer, and output collection.
Each JSON request has a 1 MiB limit.
An oversized request stops the job before the adapter starts.

Each job can write a maximum of 1 MiB to standard output and 1 MiB to standard error.
sofka stops a job that exceeds either limit.
It limits the combined displayed output to 1 MiB and 5,000 lines, plus a truncation message.
It releases each job's captured bytes after it processes the result.

The adapter must also limit its own tool output and memory use.
For large scanner results, read the tool output in parts.
Do not collect an unlimited result before you write the report.

Only one plugin run can be active.
A new run cancels the previous run.
A resource view change, a context change, or program exit cancels the active run.
Temporary overlays, such as help, do not cancel the run.
Enter `:plugin-cancel` to cancel a run manually.

On macOS and Linux, sofka starts each adapter in a separate process group.
Cancellation stops the adapter and child processes in that group.
Do not detach child processes or move them into another process group.

## Safety controls

Packages execute with the user's permissions.
They can access the selected object's full data and the inherited environment.
Install packages only from a source that you trust.

Read-only mode blocks plugins that can change resources or generate test traffic.
Set `network_load = true` for load-test tools, even if their HTTP requests use `GET`.
A network-load plugin always requires confirmation.
Connection count alone does not limit request rate.

Guardrails use the action name `plugin:<palette>`.
If there is no palette command, they use `plugin:<name>`.
Use `plugin:*` to match all plugins.

```toml
[[guardrails]]
contexts = ["prod-*"]
actions = ["plugin:benchmark"]
deny = true
reason = "Load tests are not permitted in this context."
```

Guardrails apply before adapter execution or port-forward creation.
They support denial, confirmation, typed confirmation, and bulk limits.
For a context plugin across all namespaces, namespace-specific rules also apply.
Plugin declarations do not create a security sandbox.
The author is responsible for an accurate `mutating` and `network_load` declaration.

## Port-forwards

A plugin can request a managed forward for a selected pod or service.
The remote port must be explicit.
sofka does not select the HTTP port or test the application protocol.

```toml
[plugin]
name = "Endpoint benchmark"
palette = "benchmark"
command = "python3"
args = ["adapter.py"]
scopes = ["pods", "services"]
output = "report"
mutating = false
network_load = true
port_forward = "${input.port}"
timeout = "2m"

[plugin.inputs.port]
type = "integer"
min = 1
max = 65535
```

sofka reuses an existing forward for the same target, namespace, and remote port.
Otherwise, it starts `kubectl port-forward` on an available local port.
The new forward binds to `127.0.0.1`.
sofka waits for the forward to become ready before it starts the adapter.

The request then contains:

```json
{
  "forward": {
    "host": "127.0.0.1",
    "local_port": 32123,
    "remote_port": 8080
  }
}
```

The adapter uses these values to construct its URL.
The adapter supplies the scheme, path, and HTTP options.
sofka removes a temporary forward when the job finishes or stops.
It does not remove an existing user forward.

An adapter can use an address from the selected object without a forward.
For this mode, omit `port_forward`.
This version does not let an adapter request a forward after execution starts.

## Test an adapter

The example includes a request fixture.
These checks do not require a Kubernetes cluster.

1. Check the manifest and required executables.

   ```sh
   sofka --validate-plugin examples/plugins/resource-summary
   ```

2. Run the adapter with the fixture.

   ```sh
   python3 examples/plugins/resource-summary/adapter.py \
     < examples/plugins/resource-summary/request.json > /tmp/sofka-report.json
   ```

3. Check the report format.

   ```sh
   sofka --validate-plugin-report /tmp/sofka-report.json
   ```

4. Install the package in a development configuration directory.
5. Enter `:reload` in sofka.
6. Run the plugin through its command or key chord.
7. Check cancellation, error output, and read-only behavior.

## Convert a built-in integration

For Popeye and Trivy, use `target = "context"` for namespace scans.
For a selected image scan, use `target = "selection"`.
Keep executable selection, tool arguments, and tool JSON interpretation in the adapter.
Write results with the common report format.

For oha, declare its inputs and set `network_load = true`.
Use the selected object for direct addresses or declare a managed port-forward.
Keep URL selection, benchmark options, and oha result interpretation in the adapter.

Do not add a tool-specific field to `App` or a tool-specific message to `Msg`.
If a required function is absent, propose a shared protocol extension.
