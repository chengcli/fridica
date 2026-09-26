# Fridica documentation

This folder holds the architecture notes and the design document.

## Design document

`fridica-design.pdf` describes the current design (PR #22 onward), organized by concern:

1. overview
2. mapping Slack concepts to the runtime
3. layers
4. context management
5. concurrency and durability
6. security
7. observed operations
8. appendix: comparison with the previous design

Numbers and charts come from Fridica's own state database.

## Layout

| Path | Contents |
|---|---|
| `architecture.md` | Short architecture overview (points to the PDF for detail) |
| `images/` | Screenshots used by the top-level README |
| `fridica-design.pdf` | The built design document |
| `rst/` | One RST file per section, `index.rst` (title page, contents, includes), and `fridica.yaml` (rst2pdf stylesheet) |
| `rst/generated/` | Tables and the `numbers.rst` substitutions written by the build; prose cites numbers as `\|name\|` and never hard-codes them |
| `scripts/` | `common.py` (paths, snapshots, anonymizer, RST helpers), `stats.py` (queries; no plotting), `diagrams.py` and `charts.py` (matplotlib), `sandbox.py` and `sandbox_probe.sh` (sandbox measurements), and `build.py` (entry point) |
| `figures/` | PNGs written by the scripts |

## Rebuilding

```sh
python -m pip install -e '.[docs]'     # matplotlib and rst2pdf
python docs/scripts/build.py           # figures, generated RST, then docs/fridica-design.pdf
```

Options:

| Option | Default | Meaning |
|---|---|---|
| `--state` | `~/.local/state/fridica/state.sqlite3` | Current database |
| `--legacy` | `~/.local/state/fridica/state.sqlite3.bak` | Legacy database; the appendix data is omitted when it is missing |
| `--config` | the default config | Machine list |
| `--no-pdf` | | Stop after figures and tables |
| `--probe-claude` | | Re-measure Claude's sandbox; this makes one small model call. Otherwise the last measurement in `rst/generated/sandbox_probe.json` is reused |

The daemon may keep running. Each database is opened read-only and copied through the SQLite backup API into a private temporary snapshot, so every number in one build comes from the same instant and nothing is written to Fridica's state.

## Sandbox measurements

Every build runs `scripts/sandbox_probe.sh`, first without a sandbox and then inside Fridica's bubblewrap confinement, using the argv from `fridica.exec.sandbox`. The probe checks namespaces, capabilities, seccomp, writes, reads, devices, the session bus, the SSH agent, Unix sockets and the network.

It writes only inside a temporary workspace and `/tmp`, never reads file contents, and uses read-only requests for the bus and the agent. Without `bwrap` the Fridica column keeps its last measurement.

## Privacy

The document contains aggregates only:

- counts, distributions, latencies and sizes;
- people anonymized as *owner* and *person A, B, …*;
- channels anonymized as *channel 1, …*;
- machines anonymized as *Node 1, 2, …* (configured order); GPU models and capability tags are kept.

It contains no message or post text and no session IDs. `build.py` refuses to finish if a Slack ID pattern or a real machine name appears in any RST file or in the PDF's text, and `tests/test_doc_stats.py` checks that the statistics themselves contain no IDs or text.
