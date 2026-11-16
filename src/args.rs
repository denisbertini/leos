//! Argument parsing: subcommand = first bare token; `--key=value` options;
//! `--flag` switches. std only, on purpose.

use std::collections::{HashMap, HashSet};

pub struct Args {
    pub sub: Option<String>,
    pub pos: Vec<String>,
    opts: HashMap<String, String>,
    flags: HashSet<String>,
}

impl Args {
    pub fn parse() -> Self {
        // options that consume the next token as their value
        const VALUED: &[&str] = &[
            "dest",
            "streams",
            "jobs",
            "backend",
            "extra-args",
            "seal",
            "marker",
            "watch",
            "max-gib",
            "older-than-hours",
        ];
        let mut a =
            Args { sub: None, pos: Vec::new(), opts: HashMap::new(), flags: HashSet::new() };
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut it = argv.into_iter();
        while let Some(tok) = it.next() {
            if let Some(rest) = tok.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    a.opts.insert(k.to_string(), v.to_string());
                } else if VALUED.contains(&rest) {
                    let v = it.next().unwrap_or_else(|| {
                        eprintln!("--{rest} needs a value");
                        std::process::exit(2);
                    });
                    a.opts.insert(rest.to_string(), v);
                } else {
                    a.flags.insert(rest.to_string());
                }
            } else if a.sub.is_none() {
                a.sub = Some(tok);
            } else {
                a.pos.push(tok);
            }
        }
        a
    }

    pub fn opt(&self, k: &str) -> Option<&str> {
        self.opts.get(k).map(|s| s.as_str())
    }

    pub fn flag(&self, k: &str) -> bool {
        self.flags.contains(k)
    }

    pub fn usize(&self, k: &str, default: usize) -> Result<usize, String> {
        match self.opt(k) {
            None => Ok(default),
            Some(v) => v
                .parse::<usize>()
                .map_err(|_| format!("--{k} must be a non-negative integer, got '{v}'")),
        }
    }

    /// positional argument (required)
    pub fn pos1(&self, what: &str) -> Result<&str, String> {
        self.pos
            .first()
            .map(|s| s.as_str())
            .ok_or_else(|| format!("missing <{what}> argument (see --help)"))
    }

    /// option, else environment variable, else error
    pub fn dest(&self) -> Result<String, String> {
        if let Some(d) = self.opt("dest") {
            return Ok(d.to_string());
        }
        if let Ok(d) = std::env::var("LEOS_DEST") {
            if !d.is_empty() {
                return Ok(d);
            }
        }
        Err("no destination: pass --dest or set $LEOS_DEST (see --help)".to_string())
    }
}
