//! msgpack-rpc over a child process's pipes.
//!
//! Three message shapes go over the wire, each an array whose first element
//! says which: `[0, id, method, params]` asks and expects an answer,
//! `[1, id, error, result]` is that answer, and `[2, method, params]` tells
//! without expecting one. Nvim accepts its whole API in either the asking or
//! the telling form, so [`Client::send`] is how anything is driven and
//! [`Client::call`] is kept for the few things whose answer is the point.
//!
//! A reader thread owns the incoming half: it hands replies back to whoever
//! is blocked on them and forwards everything else down a channel.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use rmpv::Value;

/// A message that was not a reply: nvim's `redraw` batches, and whatever
/// Lua sends back through `rpcnotify`.
pub struct Notification {
    pub method: String,
    pub params: Vec<Value>,
}

pub struct Client {
    out: Mutex<BufWriter<ChildStdin>>,
    next_id: AtomicU64,
    waiting: Waiting,
}

type Waiting = Arc<Mutex<HashMap<u64, Sender<Reply>>>>;
type Reply = Result<Value, String>;

impl Client {
    /// Takes the child's two pipes and starts reading. The channel closes
    /// when the child's output ends, which is how a caller learns it died.
    pub fn start(stdin: ChildStdin, stdout: ChildStdout) -> (Arc<Client>, Receiver<Notification>) {
        let waiting: Waiting = Arc::default();
        let client = Arc::new(Client {
            out: Mutex::new(BufWriter::new(stdin)),
            next_id: AtomicU64::new(1),
            waiting: Arc::clone(&waiting),
        });
        let (tx, rx) = channel();
        std::thread::spawn(move || read_loop(BufReader::new(stdout), &waiting, &tx));
        (client, rx)
    }

    /// Drive nvim without waiting to hear back. Anything that goes wrong
    /// surfaces the way it would for a person typing it: as an error message
    /// nvim draws on its own screen.
    pub fn send(&self, method: &str, params: Vec<Value>) -> Result<()> {
        self.write(Value::Array(vec![
            2.into(),
            method.into(),
            Value::Array(params),
        ]))
    }

    /// Ask, and block until the answer comes back.
    pub fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        self.waiting.lock().expect("rpc waiting list").insert(id, tx);
        let sent = self.write(Value::Array(vec![
            0.into(),
            id.into(),
            method.into(),
            Value::Array(params),
        ]));
        if let Err(e) = sent {
            self.waiting.lock().expect("rpc waiting list").remove(&id);
            return Err(e);
        }
        match rx.recv() {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => bail!("{method}: {message}"),
            Err(_) => bail!("{method}: nvim went away before answering"),
        }
    }

    fn write(&self, message: Value) -> Result<()> {
        let mut out = self.out.lock().map_err(|_| anyhow!("rpc writer poisoned"))?;
        rmpv::encode::write_value(&mut *out, &message)?;
        out.flush()?;
        Ok(())
    }
}

fn read_loop(mut input: BufReader<ChildStdout>, waiting: &Waiting, tx: &Sender<Notification>) {
    while let Ok(message) = rmpv::decode::read_value(&mut input) {
        let Some(parts) = message.as_array() else { continue };
        match parts.first().and_then(Value::as_u64) {
            Some(1) => {
                let Some(id) = parts.get(1).and_then(Value::as_u64) else { continue };
                let Some(sender) = waiting.lock().expect("rpc waiting list").remove(&id) else {
                    continue;
                };
                let error = parts.get(2).filter(|e| **e != Value::Nil);
                let _ = sender.send(match error {
                    Some(e) => Err(describe(e)),
                    None => Ok(parts.get(3).cloned().unwrap_or(Value::Nil)),
                });
            }
            Some(2) => {
                let Some(method) = parts.get(1).and_then(Value::as_str) else { continue };
                let params = parts.get(2).and_then(Value::as_array).cloned().unwrap_or_default();
                // A caller that has stopped listening for notifications is no
                // reason to stop reading: replies still have to be handed
                // back, and a reader that gave up here would leave every
                // later `call` blocked on an answer nobody collects.
                let _ = tx.send(Notification { method: method.to_owned(), params });
            }
            _ => {}
        }
    }
    // The pipe closed. Whoever is blocked on an answer is never getting one,
    // and finding that out here beats waiting forever.
    waiting.lock().expect("rpc waiting list").clear();
}

/// Nvim's errors arrive as `[type, message]`; anything else is shown raw.
fn describe(error: &Value) -> String {
    match error.as_array().and_then(|e| e.get(1)).and_then(Value::as_str) {
        Some(message) => message.to_owned(),
        None => format!("{error}"),
    }
}
