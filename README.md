# runtime-orbit

**Your laptop is out of RAM. The machine in the other room isn't.**

`runtime-orbit` points this machine's `docker` at *another* machine's container
runtime over SSH, and forwards published container ports back to your
`localhost` — so heavy builds and containers run over there, using its RAM, CPU
and disk, while `docker run -p 8080:80` still answers on `localhost:8080` here.

No daemon to configure, no code to sync, no wrapper around `docker`. It manages a
standard `docker context`, so every tool that respects `DOCKER_HOST` works
unchanged.

```
┌─ this machine (borrower) ──────┐        ┌─ the other machine (donor) ────┐
│  docker CLI                    │        │  Docker / OrbStack / Rancher   │
│  localhost:8080  ◄─────────────┼─ ssh ──┼─►  nginx container :8080       │
│  16 GB, mostly yours again     │        │  64 GB, doing the work         │
└────────────────────────────────┘        └────────────────────────────────┘
```

## Two roles, two commands

| Role | What it is | Command |
|---|---|---|
| **borrower** | the machine that's low on RAM — your laptop | `runtime-orbit setup --ip <donor-ip>` |
| **donor** | the machine lending its runtime — the beefy one | `runtime-orbit donor setup` |

That's the whole setup. Everything else is optional.

## Install

**macOS / Linux**

```sh
curl -fsSL https://slothlabs.org/install/runtime-orbit | sh
```

**Homebrew**

```sh
brew install slothlabsorg/tap/runtime-orbit
```

**Windows (PowerShell)**

```powershell
irm https://slothlabs.org/install/runtime-orbit.ps1 | iex
```

Installs `runtime-orbit`, plus `r-orbit` and `orbit` as shortcuts. Prebuilt
binaries for macOS (Intel + Apple Silicon), Linux (x86_64 + arm64) and Windows
are on the [releases page](https://github.com/slothlabsorg/runtime-orbit/releases).

## Two minutes, start to finish

On the **donor** — the machine with the RAM:

```sh
runtime-orbit donor setup
```

It checks the runtime is up, offers to switch on the SSH server and stop the
machine sleeping (both need your admin password, both asked for right there), and
prints the exact command for the other machine.

On the **borrower** — the machine that needs help:

```sh
runtime-orbit setup --ip 192.168.1.20
```

It authorizes itself on the donor — either with the donor's login password, typed
once inside the command, or with a 6-digit pairing code so no password is needed
at all — then links, routes docker over, and proves it works by running nginx on
the donor and curling it through your localhost.

Nothing to copy, paste, or edit by hand. From then on:

```sh
docker compose up          # runs on the donor
curl localhost:8080        # answers from the donor
runtime-orbit dashboard    # watch both machines live
runtime-orbit down         # back to local docker
```

## The dashboard

```
runtime-orbit  borrowing   14:32:07

  THIS MACHINE                          DONOR
  macbook-dany                          dany@192.168.1.20
  macOS 26.2 · arm64                    macOS 15.4 · arm64
  192.168.1.8 · 10 cores                192.168.1.20 · 16 cores
  RAM ███░░░░░░░  31%  7.4/24.0 GB      RAM ██░░░░░░░░  18%  11.8/64.0 GB
  load 1.82 · up 3 days                 load 0.44 · up 12 days

  ROUTING
  docker context     runtime-orbit → donor
  ssh                dany@192.168.1.20:22 · master up · forwarder up
  ports              localhost:8080  localhost:5432

  BORROWED RIGHT NOW
  carried by donor   8.4 GiB · 212% CPU · 6 container(s)
  on this machine    0 container(s)

  CONTAINER              IMAGE                          CPU        MEM     PORTS
  api                    acme/api:dev                 42.1%    1.2 GiB      8080
  postgres               postgres:16                   3.4%    0.8 GiB      5432

  TRAFFIC
  containers         ↓ 1.2 MB/s   ↑ 340 kB/s   (1.4 GB in / 220 MB out total)
  donor network      ↓ 4.1 MB/s   ↑ 900 kB/s

  → 8.4 GiB of RAM and 212% CPU are on 192.168.1.20 instead of here ♥
```

`--once` for a single frame, `--json` for a machine-readable snapshot.

## Every command

**On the borrower** (this machine)

| Command | What it does |
|---|---|
| `setup --ip <addr>` | the whole thing: authorize, link, route, self-test |
| `doctor` | check every link in the chain, with a fix for each failure |
| `up` / `down` | start / stop routing docker to the donor |
| `status` | one-shot summary |
| `dashboard` | live view of both machines (`--once`, `--json`, `-n <secs>`) |
| `ports [add\|rm <port>]` | list or hand-manage forwarded TCP ports |
| `engines` | which container runtimes exist on each machine |
| `logs [-f]` | the port-forwarder's log |
| `service install` | keep the borrow alive across logins (launchd/systemd) |
| `pair` | offer this machine's key to a donor, no password |
| `link <user@host>` | low-level link; `setup` calls this for you |

**On the donor** (the beefy machine) — `donator` and `lender` also work

| Command | What it does |
|---|---|
| `donor setup` | prepare to lend: runtime, SSH, sleep, pending requests |
| `donor doctor` | can this machine lend? what's in the way? |
| `donor status` | what it's lending right now, and to whom |
| `donor pair <borrower-ip>` | pull a borrower's key over the LAN and authorize it |
| `donor pending` | review pairing requests waiting for approval |
| `donor allow "<pubkey>"` | authorize a key directly (`--iphost` pins it to one IP) |

**Routing policy** — for when you don't want *everything* delegated

| Command | What it does |
|---|---|
| `limits show` | budgets, and where the next container would land |
| `limits set --max-ram 32 --local-ram-threshold 5` | borrow at most 32 GB; stay local until 5 GB is used here |
| `route list` / `route add` / `route rm` | the routing table |
| `route explain postgres:16` | why a given workload goes where it goes |
| `docker <args>` | run one docker command through the table |

**Other**

| Command | What it does |
|---|---|
| `mcp` | MCP server (stdio) so AI assistants can drive it |
| `funding` | it's free; here's where to chip in |

## Keeping some things local

Delegating everything is the default because it's usually right. When it isn't —
a database whose disk latency you care about, a container that talks to a USB
device — the routing table decides per workload:

```sh
runtime-orbit limits set --local-ram-threshold 5 --max-ram 32
runtime-orbit route add 'postgres:*' --target local --note 'disk latency'
runtime-orbit route add '*' --target donor

runtime-orbit docker run -d postgres:16    # stays here
runtime-orbit docker build -t api:dev .    # goes to the donor
```

Rules are evaluated top to bottom, first match wins; anything unmatched falls
through to the budgets. `route explain <image>` tells you which rule fired and
why.

## Requirements

- **Both machines on the same LAN**, and an SSH server on the donor (`donor
  setup` can switch it on).
- **A container runtime on the donor**: Docker Desktop, OrbStack, Rancher
  Desktop, colima, Lima, Podman or containerd — anything speaking the Docker
  Engine API over a unix socket.
- **The `docker` CLI on the borrower.** No engine needed here; that's the point.
- macOS and Linux are fully supported on both sides. A Windows donor works via
  WSL2 — run `donor setup` inside the distro.

## Docs

Full guide, architecture diagrams and troubleshooting:

- [`docs/GUIDE.md`](docs/GUIDE.md) — install, both roles, every command, diagrams
- [slothlabs.org/runtime-orbit/docs](https://slothlabs.org/runtime-orbit/docs)

## Built with love

runtime-orbit is free and open source, from [SlothLabs](https://slothlabs.org) —
no company, no VC, just developers fixing their own friction so your laptop stops
choking on Docker. If it saves you time and RAM,
[supporting the work](https://slothlabs.org/pricing) keeps the tools coming. ♥

MIT licensed.
