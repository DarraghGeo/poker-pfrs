# pfrs

`pfrs` is a small solve server for postflop Texas Hold'em, built around
[b-inary/postflop-solver](https://github.com/b-inary/postflop-solver). It runs as its own program
and is spoken to in JSON lines on stdin and stdout. Its first user is a web app that starts one
`pfrs` process per solve.

## Credit and licence

The solver is **postflop-solver** by Wataru Inariba (b-inary), copied unmodified from commit
[`9d1509f`](https://github.com/b-inary/postflop-solver/tree/9d1509fe5077d019825f833eed04b16d342dfda1)
into [`postflop-solver/`](postflop-solver). All the solving is its work. This repo adds only the
adapter in [`src/main.rs`](src/main.rs).

postflop-solver is licensed under the GNU Affero General Public License v3.0 or later, and so is
`pfrs` ([LICENSE](LICENSE)). If you run a modified `pfrs` behind a network service, the AGPL
requires you to offer that service's users its source.

## Build

```sh
cargo build --release
```

This needs stable Rust. `.cargo/config.toml` allows one lint (`dangerous_implicit_autorefs`) that
newer compilers deny by default and that postflop-solver's 2023 source predates. The binary is
`target/release/pfrs`. `pfrs --version` prints its version and the postflop-solver commit.

## Protocol

One process holds one solve. Each command is one JSON object on its own line.

| Command | Does |
|---|---|
| `{"cmd":"solve","spot":{…},"target":0.5,"maxIterations":1000,"threads":4,"maxBytes":1e9}` | Builds the tree and solves until exploitability is at most `target` % of the pot, or `maxIterations` are done. Above `maxBytes`, it solves with 16-bit storage. |
| `{"cmd":"stop"}` | Ends the solve after the current iteration and keeps the partial answer. |
| `{"cmd":"node","id":1,"path":[0,"Kh",1]}` | Describes the node at the end of a line: action indices at decisions, card names at chance nodes. |
| `{"cmd":"dump","id":2}` | Streams the average strategy of every decision on every runout. |
| `{"cmd":"quit"}`, or end of input | Exits once the commands already sent are done. |

Events come back one per line:

| Event | Carries |
|---|---|
| `ready` | Version and solver commit. |
| `built` | Threads, memory, whether it is compressed, and each player's hands in solver order. |
| `progress` | Iteration, exploitability (% of the pot, `null` until measured), elapsed ms. |
| `done` | Iterations, exploitability, `hitTarget`, `stopped`, each player's EV (chips they end with, so the two add up to the pot). |
| `node` | Actor, action labels, chips each player has put in, and per hand: reach, equity, EV, plus the actor's strategy and EV per action. |
| `strategy` | One per decision and runout: the line `h`, actor `p`, labels `a`, and the strategy `s` as base64 little-endian float32, actions × hands. |
| `dead` | At a chance node: cards never dealt there, because no pair of hands could share them. |
| `dumped` | The end of a dump. |
| `error` | `message`, and the `id` of the command that failed. |

Action labels are `F`, `X`, `C`, `B<n>`, `R<n>` and `A<n>`, where `n` is the actor's total on this
street in hundredths of a chip.

Logs go to stderr. `pfrs` writes no files.

### The spot

```json
{
  "board": "Ks7d2c",
  "pot": 10, "stack": 95,
  "oop": "AA,KK,AKs,AsKh:0.5",
  "ip": "QQ-99,AQs",
  "streets": [
    {"bets": [33, 75], "raise": 3, "raises": 1, "allIn": true},
    {"bets": [75], "raise": 3, "raises": 1, "allIn": true},
    {"bets": [75], "raise": 3, "raises": 0, "allIn": true}
  ]
}
```

- **Ranges** are weighted single combos (`AsKh:0.5`) or postflop-solver range notation.
- **`streets`** has one entry per street, from the board's own street to the river. Without it,
  flat `bets`, `raise`, `raises` and `allIn` apply to every street.
- **`bets`** are % of the pot after calling.
- **`raise: N`** raises to N times the bet faced.
- **`raises`** is how many raises are allowed after the opening bet on that street.
- **`allIn`** offers a shove beside the sizes.

Chips are scaled by 100 to integers. The tree has no automatic all-in, no forced all-in and no
merging, and donk bets use the normal sizes. Two rules are added on top of postflop-solver's tree:

- Aggressive actions past 1 + `raises` on a street are removed.
- An all-in raise smaller than a legal raise is removed.

## Example

```sh
printf '%s\n' \
  '{"cmd":"solve","spot":{"board":"Ks7d2c5h9s","pot":10,"stack":50,"oop":"AA,KK,AK,KQ,JT","ip":"AA,AK,KQ,QQ,T9","bets":[50,100],"raise":3,"raises":1,"allIn":true},"target":0.3,"maxIterations":500,"threads":2}' \
  '{"cmd":"node","id":1,"path":[]}' | ./target/release/pfrs
```

`examples/river.json` holds the same spot.
