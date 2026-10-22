// pfrs: a solve server around postflop-solver, spoken to in JSON lines on stdin and stdout.
//
// One process holds one solve. Commands in (one JSON object per line):
//   {"cmd":"solve","spot":{...},"target":0.5,"maxIterations":1000,"threads":3,"maxBytes":1e9}
//   {"cmd":"stop"}                         end the solve after the current iteration
//   {"cmd":"node","id":1,"path":[0,"Kh",1]} actions by index, cards by name at chance nodes
//   {"cmd":"dump","id":2}                  the average strategy of every decision on every runout
//   {"cmd":"quit"}                         or end of input: exit once queued commands are done
// Events out: ready, built, progress, done, node, strategy (many) then dumped, error.
// Logs go to stderr. Nothing is written to disk.
//
// The spot (see README.md): board, pot, stack, oop, ip, and betting either flat
// (bets, raise, raises, allIn) or per street from the board's own (streets: [...]).
//   bets    % of the pot after calling, rounded to 0.01 chip (chips are scaled x100)
//   raise   N: raise to N times the bet faced on this street
//   raises  raises allowed after the opening bet on each street
//   allIn   offer a shove beside the sizes
// No automatic all-in, no forced all-in, no merging; donk bets use the normal bet sizes.
// postflop-solver has no raise cap, so aggressive actions past 1 + raises on a street are
// removed from the action tree, as are all-in raises smaller than a legal raise.

use postflop_solver::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

const SCALE: f64 = 100.0;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SOLVER_COMMIT: &str = "9d1509f";

static STOP: AtomicBool = AtomicBool::new(false);

fn emit(v: &Value) {
    let out = std::io::stdout();
    let mut lock = out.lock();
    if writeln!(lock, "{v}").and_then(|_| lock.flush()).is_err() {
        std::process::exit(0);
    }
}

fn fail(id: &Value, message: impl Into<String>) {
    emit(&json!({"type": "error", "id": id, "message": message.into()}));
}

// ------------------------------------------------------------------ spot

struct Street {
    bets: Vec<f64>,
    raise: f64,
    raises: usize,
    all_in: bool,
}

fn num(v: &Value, name: &str, lo: f64, hi: f64) -> Result<f64, String> {
    let x = v.as_f64().ok_or(format!("{name} must be a number"))?;
    if !(x >= lo && x <= hi) {
        return Err(format!("{name} must be from {lo} to {hi}"));
    }
    Ok(x)
}

fn street_of(v: &Value) -> Result<Street, String> {
    let bets = v["bets"].as_array().ok_or("bets must be a list")?;
    if bets.len() > 3 {
        return Err("at most three bet sizes per street".into());
    }
    Ok(Street {
        bets: bets.iter().map(|x| num(x, "bet size", 1.0, 500.0)).collect::<Result<_, _>>()?,
        raise: if v["raise"].is_null() { 3.0 } else { num(&v["raise"], "raise", 1.5, 20.0)? },
        raises: if v["raises"].is_null() { 1 } else { num(&v["raises"], "raises", 0.0, 3.0)? as usize },
        all_in: v["allIn"].as_bool().unwrap_or(false),
    })
}

fn sizes(s: &Street) -> Result<BetSizeOptions, String> {
    let mut bet: Vec<String> = s.bets.iter().map(|b| format!("{b}%")).collect();
    let mut raise = vec![format!("{}x", s.raise)];
    if s.all_in {
        bet.push("a".into());
        raise.push("a".into());
    }
    BetSizeOptions::try_from((bet.join(",").as_str(), raise.join(",").as_str()))
}

// Weighted single combinations ("AsKh:0.6,QdQc") or postflop-solver range notation.
fn parse_range(s: &str) -> Result<Range, String> {
    let items: Vec<&str> = s.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).collect();
    let combos = items.iter().all(|item| item.split(':').next().map_or(false, |h| h.len() == 4));
    if !combos {
        return s.parse::<Range>();
    }
    let mut r = Range::new();
    for item in items {
        let (h, w) = match item.split_once(':') {
            Some((h, w)) => (h, w.trim().parse::<f32>().map_err(|_| format!("bad weight in {item}"))?),
            None => (item, 1.0),
        };
        let a = card_from_str(&h[0..2])?;
        let b = card_from_str(&h[2..4])?;
        if a == b {
            return Err(format!("{item} repeats a card"));
        }
        r.set_weight_by_cards(a, b, w.clamp(0.0, 1.0));
    }
    Ok(r)
}

fn is_aggr(a: &Action) -> bool {
    matches!(a, Action::Bet(_) | Action::Raise(_) | Action::AllIn(_))
}

// Make the action tree visolver's (solver/tree.ts):
//  - at most 1 + raises aggressive actions per street;
//  - no all-in raise smaller than a legal raise (the last bet or raise on the street, and at
//    least one chip): visolver leaves those out, where postflop-solver offers them.
// `facing` is the street total to call, `last` the size of the last bet or raise (chips x 100).
fn prune(t: &mut ActionTree, street: usize, aggr: usize, facing: i32, last: i32, cap: &[usize; 3]) {
    if t.is_terminal_node() {
        return;
    }
    let acts: Vec<Action> = t.available_actions().to_vec();
    let mut keep = Vec::new();
    for a in acts {
        let short_all_in = matches!(a, Action::AllIn(x) if facing > 0 && x - facing < last.max(SCALE as i32));
        if (is_aggr(&a) && aggr >= cap[street]) || short_all_in {
            t.remove_action(a).unwrap();
        } else {
            keep.push(a);
        }
    }
    for a in keep {
        t.play(a).unwrap();
        if !t.is_terminal_node() {
            if t.is_chance_node() {
                prune(t, street + 1, 0, 0, 0, cap);
            } else {
                let (f, l) = match a {
                    Action::Bet(x) | Action::Raise(x) | Action::AllIn(x) => (x, x - facing),
                    _ => (facing, last),
                };
                prune(t, street, aggr + is_aggr(&a) as usize, f, l, cap);
            }
        }
        t.undo().unwrap();
    }
}

// Amounts are this street's total for the actor, in chips x 100.
fn label(a: &Action) -> String {
    match a {
        Action::Fold => "F".into(),
        Action::Check => "X".into(),
        Action::Call => "C".into(),
        Action::Bet(x) => format!("B{x}"),
        Action::Raise(x) => format!("R{x}"),
        Action::AllIn(x) => format!("A{x}"),
        _ => "?".into(),
    }
}

fn build(spot: &Value, max_bytes: f64) -> Result<(PostFlopGame, i32, i32, bool), String> {
    let board = spot["board"].as_str().ok_or("board is required")?.replace([' ', ','], "");
    if board.len() % 2 != 0 || !(6..=10).contains(&board.len()) {
        return Err("board needs 3, 4 or 5 cards".into());
    }
    let cards: Vec<u8> = (0..board.len() / 2).map(|i| card_from_str(&board[2 * i..2 * i + 2])).collect::<Result<_, _>>()?;
    for i in 0..cards.len() {
        if cards[..i].contains(&cards[i]) {
            return Err("the board repeats a card".into());
        }
    }
    let rounds = 6 - cards.len();
    let first = cards.len() - 3; // 0 flop, 1 turn, 2 river
    let streets: Vec<Street> = match spot["streets"].as_array() {
        Some(a) => a.iter().map(street_of).collect::<Result<_, _>>()?,
        None => (0..rounds).map(|_| street_of(spot)).collect::<Result<_, _>>()?,
    };
    if streets.len() != rounds {
        return Err(format!("give betting for each of the {rounds} streets"));
    }
    let mut cap = [9usize; 3];
    for abs in first..3 {
        cap[abs] = 1 + streets[abs - first].raises;
    }
    let sz = |abs: usize| -> Result<BetSizeOptions, String> {
        if abs >= first { sizes(&streets[abs - first]) } else { Ok(BetSizeOptions::default()) }
    };
    let pot = (num(&spot["pot"], "pot", 0.01, 1e6)? * SCALE).round() as i32;
    let stack = (num(&spot["stack"], "stack", 0.01, 1e6)? * SCALE).round() as i32;
    let tc = TreeConfig {
        initial_state: [BoardState::Flop, BoardState::Turn, BoardState::River][first],
        starting_pot: pot,
        effective_stack: stack,
        rake_rate: 0.0,
        rake_cap: 0.0,
        flop_bet_sizes: [sz(0)?, sz(0)?],
        turn_bet_sizes: [sz(1)?, sz(1)?],
        river_bet_sizes: [sz(2)?, sz(2)?],
        turn_donk_sizes: None,
        river_donk_sizes: None,
        add_allin_threshold: 0.0,
        force_allin_threshold: 0.0,
        merging_threshold: 0.0,
    };
    let cc = CardConfig {
        range: [
            parse_range(spot["oop"].as_str().ok_or("oop range is required")?)?,
            parse_range(spot["ip"].as_str().ok_or("ip range is required")?)?,
        ],
        flop: [cards[0], cards[1], cards[2]],
        turn: if cards.len() > 3 { cards[3] } else { NOT_DEALT },
        river: if cards.len() > 4 { cards[4] } else { NOT_DEALT },
    };
    let mut tree = ActionTree::new(tc)?;
    prune(&mut tree, first, 0, 0, 0, &cap);
    tree.back_to_root();
    let mut game = PostFlopGame::with_config(cc, tree)?;
    let (raw, compressed) = game.memory_usage();
    let compress = raw as f64 > max_bytes;
    if compress && compressed as f64 > max_bytes {
        return Err(format!("needs {} MB even compressed; the limit is {} MB", compressed >> 20, (max_bytes as u64) >> 20));
    }
    game.allocate_memory(compress);
    Ok((game, pot, stack, compress))
}

// ------------------------------------------------------------------ solving

struct Solved {
    game: PostFlopGame,
    pot: i32,
    stack: i32,
}

fn solve(cmd: &Value) -> Result<Solved, String> {
    let threads = cmd["threads"].as_u64().unwrap_or(1).clamp(1, 64) as usize;
    let _ = rayon::ThreadPoolBuilder::new().num_threads(threads).build_global();
    let target_pct = num(&cmd["target"], "target", 0.0, 100.0).unwrap_or(0.5);
    let max_iter = cmd["maxIterations"].as_u64().unwrap_or(1000).clamp(1, 100_000) as u32;
    let max_bytes = cmd["maxBytes"].as_f64().unwrap_or(f64::INFINITY);

    let t0 = Instant::now();
    let (mut game, pot, stack, compressed) = build(&cmd["spot"], max_bytes)?;
    let (raw, comp) = game.memory_usage();
    let hands: Vec<Vec<String>> = (0..2).map(|p| holes_to_strings(game.private_cards(p)).unwrap()).collect();
    emit(&json!({
        "type": "built", "threads": rayon::current_num_threads(), "compressed": compressed,
        "bytes": if compressed { comp } else { raw }, "memoryUsage": [raw, comp],
        "buildMs": t0.elapsed().as_millis() as u64, "hands": hands,
    }));

    let pct = |e: f32| e as f64 / pot as f64 * 100.0;
    let started = Instant::now();
    let mut last_emit = Instant::now();
    let mut exploit = f32::NAN;
    let mut measured_at = 0u32;
    let mut next = max_iter.min(10);
    let mut hit = false;
    let mut t = 0u32;
    while t < max_iter && !STOP.load(Ordering::Relaxed) {
        solve_step(&game, t);
        t += 1;
        if t >= next || t == max_iter {
            exploit = compute_exploitability(&game);
            measured_at = t;
            if pct(exploit) <= target_pct {
                hit = true;
            }
            // Exploitability falls roughly as 1/T: aim a little short of the predicted crossing,
            // measuring at least every doubling and at most every 10% of iterations.
            let aim = t as f64 * pct(exploit) / target_pct.max(1e-9) * 0.95;
            next = (aim.clamp(t as f64 * 1.1, t as f64 * 2.0).ceil() as u32).max(t + 1);
            emit(&json!({"type": "progress", "iteration": t, "exploitability": pct(exploit), "elapsedMs": started.elapsed().as_millis() as u64}));
            last_emit = Instant::now();
            if hit {
                break;
            }
        } else if last_emit.elapsed().as_millis() >= 250 {
            emit(&json!({"type": "progress", "iteration": t, "exploitability": if exploit.is_nan() { Value::Null } else { json!(pct(exploit)) }, "elapsedMs": started.elapsed().as_millis() as u64}));
            last_emit = Instant::now();
        }
    }
    if measured_at != t {
        exploit = compute_exploitability(&game);
        hit = pct(exploit) <= target_pct;
    }
    let elapsed = started.elapsed().as_millis() as u64;
    finalize(&mut game);

    game.back_to_root();
    game.cache_normalized_weights();
    let ev: Vec<f64> = (0..2).map(|p| compute_average(&game.expected_values(p), game.normalized_weights(p)) as f64 / SCALE).collect();
    emit(&json!({
        "type": "done", "iterations": t, "exploitability": pct(exploit), "hitTarget": hit,
        "stopped": STOP.load(Ordering::Relaxed) && !hit, "elapsedMs": elapsed, "ev": ev,
        "targetPercent": target_pct,
    }));
    Ok(Solved { game, pot, stack })
}

// ------------------------------------------------------------------ queries

// A path of action indices and card names, as postflop-solver history.
fn history_of(game: &mut PostFlopGame, path: &Value) -> Result<Vec<usize>, String> {
    let steps = path.as_array().ok_or("path must be a list")?;
    game.back_to_root();
    let mut h = vec![];
    for s in steps {
        if game.is_terminal_node() {
            return Err("the hand is over at this point".into());
        }
        let step = if game.is_chance_node() {
            let c = card_from_str(s.as_str().ok_or("a card has to be dealt here")?)?;
            if game.possible_cards() & (1u64 << c) == 0 {
                return Err("that card cannot come".into());
            }
            c as usize
        } else {
            let i = s.as_u64().ok_or("an action index is needed here")? as usize;
            if i >= game.available_actions().len() {
                return Err("that action does not exist here".into());
            }
            i
        };
        game.play(step);
        h.push(step);
    }
    Ok(h)
}

fn round6(x: f32) -> f64 {
    (x as f64 * 1e6).round() / 1e6
}

fn node(s: &mut Solved, id: &Value, path: &Value) -> Result<Value, String> {
    let game = &mut s.game;
    history_of(game, path)?;
    let tb = game.total_bet_amount();
    let board: Vec<String> = game.current_board().iter().map(|&c| card_to_string(c).unwrap()).collect();
    let pot_now = (s.pot + tb[0] + tb[1]) as f64 / SCALE;
    if game.is_terminal_node() {
        return Ok(json!({"type": "node", "id": id, "kind": "terminal", "board": board, "pot": pot_now}));
    }
    if game.is_chance_node() {
        let mask = game.possible_cards();
        let cards: Vec<String> = (0..52u8).filter(|c| mask & (1u64 << c) != 0).map(|c| card_to_string(c).unwrap()).collect();
        return Ok(json!({"type": "node", "id": id, "kind": "chance", "board": board, "pot": pot_now, "dealable": cards}));
    }
    game.cache_normalized_weights();
    let actor = game.current_player();
    let actions: Vec<String> = game.available_actions().iter().map(label).collect();
    let per = |v: Vec<f32>| -> Vec<f64> { v.into_iter().map(round6).collect() };
    let players: Vec<Value> = (0..2)
        .map(|p| {
            json!({
                "reach": per(game.weights(p).to_vec()),
                "equity": per(game.equity(p)),
                // Chips each hand ends with from here, under the average strategy (pot share won).
                "ev": per(game.expected_values(p).iter().map(|x| x / SCALE as f32).collect()),
            })
        })
        .collect();
    Ok(json!({
        "type": "node", "id": id, "kind": "decision", "board": board, "pot": pot_now, "actor": actor,
        "actions": actions, "invested": [tb[0] as f64 / SCALE, tb[1] as f64 / SCALE], "stack": s.stack as f64 / SCALE,
        "players": players,
        // Row-major actions x hands of the actor, as postflop-solver lays them out.
        "strategy": per(game.strategy()),
        "actionEv": per(game.expected_values_detail(actor).iter().map(|x| x / SCALE as f32).collect()),
    }))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

// Every decision on every runout, depth first, parents before children. Each record carries its
// line (action indices, card names), the actor, the action labels and the strategy as
// little-endian f32, actions x hands. Runouts after an all-in hold no decisions and are skipped.
fn dump(s: &mut Solved, id: &Value) {
    let game = &mut s.game;
    let out = std::io::stdout();
    let mut w = BufWriter::with_capacity(1 << 20, out.lock());
    let mut todo: Vec<(Vec<usize>, Vec<Value>)> = vec![(vec![], vec![])];
    let mut records = 0u64;
    while let Some((h, path)) = todo.pop() {
        game.apply_history(&h);
        if game.is_terminal_node() {
            continue;
        }
        if game.is_chance_node() {
            let tb = game.total_bet_amount();
            if tb[0] >= s.stack || tb[1] >= s.stack {
                continue;
            }
            let mask = game.possible_cards();
            // Cards no pair of hands can share are never dealt here; say so, as no decisions follow them.
            let board = game.current_board();
            let dead: Vec<String> = (0..52u8)
                .filter(|c| mask & (1u64 << c) == 0 && !board.contains(c))
                .map(|c| card_to_string(c).unwrap())
                .collect();
            if !dead.is_empty() {
                if writeln!(w, "{}", json!({"type": "dead", "id": id, "h": path, "cards": dead})).is_err() {
                    std::process::exit(0);
                }
            }
            for c in (0..52u8).rev().filter(|c| mask & (1u64 << c) != 0) {
                let mut h2 = h.clone();
                h2.push(c as usize);
                let mut p2 = path.clone();
                p2.push(json!(card_to_string(c).unwrap()));
                todo.push((h2, p2));
            }
            continue;
        }
        let acts = game.available_actions();
        let strat = game.strategy();
        let mut bytes = Vec::with_capacity(strat.len() * 4);
        for x in &strat {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        let rec = json!({
            "type": "strategy", "id": id, "h": path, "p": game.current_player(),
            "a": acts.iter().map(label).collect::<Vec<_>>(), "s": base64(&bytes),
        });
        if writeln!(w, "{rec}").is_err() {
            std::process::exit(0);
        }
        records += 1;
        for i in (0..acts.len()).rev() {
            let mut h2 = h.clone();
            h2.push(i);
            let mut p2 = path.clone();
            p2.push(json!(i));
            todo.push((h2, p2));
        }
    }
    game.back_to_root();
    if writeln!(w, "{}", json!({"type": "dumped", "id": id, "records": records})).and_then(|_| w.flush()).is_err() {
        std::process::exit(0);
    }
}

// ------------------------------------------------------------------ main

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("pfrs {VERSION} (postflop-solver {SOLVER_COMMIT})");
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        eprintln!("pfrs panic: {msg}");
        emit(&json!({"type": "error", "id": null, "message": format!("solver crashed: {msg}"), "fatal": true}));
    }));

    // Commands are read on their own thread so a stop lands while the main thread is solving.
    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if v["cmd"] == "stop" {
                        STOP.store(true, Ordering::Relaxed);
                    }
                    if v["cmd"] == "quit" || tx.send(v).is_err() {
                        break;
                    }
                }
                Err(e) => fail(&Value::Null, format!("bad command: {e}")),
            }
        }
        // End of input or quit: the main thread finishes what is queued, then exits. A host that
        // vanishes mid-solve ends this process at the next progress line (a broken pipe).
    });

    emit(&json!({"type": "ready", "version": VERSION, "solver": {"name": "postflop-solver", "repo": "https://github.com/b-inary/postflop-solver", "commit": SOLVER_COMMIT}}));
    let mut solved: Option<Solved> = None;
    let mut solving_done = false;
    for cmd in rx {
        let id = cmd["id"].clone();
        match cmd["cmd"].as_str().unwrap_or("") {
            "solve" => {
                if solving_done {
                    fail(&id, "this process has already solved a spot; start another");
                    continue;
                }
                solving_done = true;
                match solve(&cmd) {
                    Ok(s) => solved = Some(s),
                    Err(e) => fail(&id, e),
                }
            }
            "stop" => {}
            "node" => match solved.as_mut() {
                None => fail(&id, "the solve has not finished"),
                Some(s) => match node(s, &id, &cmd["path"]) {
                    Ok(v) => emit(&v),
                    Err(e) => fail(&id, e),
                },
            },
            "dump" => match solved.as_mut() {
                None => fail(&id, "the solve has not finished"),
                Some(s) => dump(s, &id),
            },
            other => fail(&id, format!("unknown command {other:?}")),
        }
    }
}
