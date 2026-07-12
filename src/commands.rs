//! A small scripting layer for chat commands.
//!
//! This module owns parsing (via the grammar in `commands.pest`) and
//! interpretation (control flow: `alias`, `if`/`&&`/`||`/comparisons,
//! `load`, `$N` argument substitution, recursion). It deliberately knows
//! nothing about IRC or networking -- any command it doesn't recognize as
//! one of its own control-flow builtins is handed to whatever implements
//! [`CommandHost`], which is where `connect`, `msg`, `echo`, etc. actually
//! live (in `main.rs`).
//!
//! Parsing is done with `pest` rather than hand-written, mainly for two
//! things that are painful to hand-roll correctly: operator precedence in
//! `if` conditions (`||` binds looser than `&&`, which binds looser than
//! `==`/`<`/etc.) and source-located error messages (`pest::error::Error`
//! renders a line/column + caret pointing at the problem for free).
//!
//! A leading `/` on a command name is always stripped, so `/connect` and
//! `connect` are equivalent everywhere -- callers decide whether they
//! *require* the slash (the top-level chat input does; statements inside a
//! `{ }` block don't need to).

use std::collections::HashMap;

use pest::iterators::Pair;
use pest::Parser;
use pest_derive::Parser as PestParser;

#[derive(PestParser)]
#[grammar = "commands.pest"]
struct CommandParser;

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// One parsed statement.
#[derive(Debug, Clone)]
enum Stmt {
    /// `if <expr> { block }`. A real grammar-level construct (not just a
    /// command named "if") since its argument is a whole expression, not a
    /// flat word list.
    If { cond: Expr, block: Vec<Stmt> },
    /// Every other command: `name arg* block?`. What `alias`/`load`/a
    /// user-defined alias/an unrecognized name each *mean* is entirely up
    /// to the interpreter -- the parser treats them identically.
    Command {
        name: String,
        args: Vec<String>,
        block: Option<Vec<Stmt>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// An `if` condition. Leaves hold raw (not-yet-`$N`-substituted) text --
/// substitution happens at evaluation time, using whatever scope the `if`
/// is running in.
#[derive(Debug, Clone)]
enum Expr {
    /// A bare value with no comparison: truthy if the substituted string is
    /// non-empty and isn't "0" or "false" (case-insensitive).
    Atom(String),
    Cmp(String, CmpOp, String),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

/// Public parsed-script handle. Opaque outside this module -- callers just
/// get one from [`parse_script`] and hand it to [`Interpreter::exec`].
pub struct Script(Vec<Stmt>);

// ---------------------------------------------------------------------------
// Parsing: pest pairs -> AST
// ---------------------------------------------------------------------------

/// Parses a whole script -- one input line, or a whole file's contents --
/// into something [`Interpreter::exec`] can run. Returns pest's own error
/// message on failure, which includes a line/column and a caret pointing at
/// the problem.
pub fn parse_script(text: &str) -> Result<Script, String> {
    let mut pairs = CommandParser::parse(Rule::script, text).map_err(|e| e.to_string())?;
    let script_pair = pairs.next().ok_or_else(|| "empty script".to_string())?;
    Ok(Script(build_block_body(script_pair)?))
}

/// Shared by `script` and `block` -- both are just "zero or more
/// statements" as far as the grammar's inner structure goes.
fn build_block_body(pair: Pair<Rule>) -> Result<Vec<Stmt>, String> {
    let mut stmts = Vec::new();
    for inner in pair.into_inner() {
        if inner.as_rule() == Rule::statement {
            stmts.push(build_statement(inner)?);
        }
        // Silently skip EOI and anything else -- terminators are already
        // silent at the grammar level.
    }
    Ok(stmts)
}

fn build_statement(pair: Pair<Rule>) -> Result<Stmt, String> {
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| "empty statement".to_string())?;
    match inner.as_rule() {
        Rule::if_stmt => build_if(inner),
        Rule::generic_stmt => build_generic(inner),
        other => Err(format!("unexpected statement kind: {other:?}")),
    }
}

fn build_if(pair: Pair<Rule>) -> Result<Stmt, String> {
    let mut inner = pair.into_inner();
    let expr_pair = inner.next().ok_or_else(|| "if: missing condition".to_string())?;
    let block_pair = inner.next().ok_or_else(|| "if: missing { block }".to_string())?;
    Ok(Stmt::If {
        cond: build_expr(expr_pair)?,
        block: build_block_body(block_pair)?,
    })
}

fn build_generic(pair: Pair<Rule>) -> Result<Stmt, String> {
    let mut inner = pair.into_inner();
    let name = inner
        .next()
        .ok_or_else(|| "missing command name".to_string())?
        .as_str()
        .to_string();

    let mut args = Vec::new();
    let mut block = None;
    for p in inner {
        match p.as_rule() {
            Rule::arg => args.push(unquote(p.as_str())),
            Rule::block => block = Some(build_block_body(p)?),
            other => return Err(format!("unexpected token in statement: {other:?}")),
        }
    }
    Ok(Stmt::Command { name, args, block })
}

fn build_expr(pair: Pair<Rule>) -> Result<Expr, String> {
    let or_pair = pair
        .into_inner()
        .next()
        .ok_or_else(|| "empty expression".to_string())?;
    build_or(or_pair)
}

fn build_or(pair: Pair<Rule>) -> Result<Expr, String> {
    let mut inner = pair.into_inner();
    let mut expr = build_and(inner.next().ok_or_else(|| "empty expression".to_string())?)?;
    while let Some(_op) = inner.next() {
        let rhs = build_and(inner.next().ok_or_else(|| "'||' missing right-hand side".to_string())?)?;
        expr = Expr::Or(Box::new(expr), Box::new(rhs));
    }
    Ok(expr)
}

fn build_and(pair: Pair<Rule>) -> Result<Expr, String> {
    let mut inner = pair.into_inner();
    let mut expr = build_cmp(inner.next().ok_or_else(|| "empty expression".to_string())?)?;
    while let Some(_op) = inner.next() {
        let rhs = build_cmp(inner.next().ok_or_else(|| "'&&' missing right-hand side".to_string())?)?;
        expr = Expr::And(Box::new(expr), Box::new(rhs));
    }
    Ok(expr)
}

fn build_cmp(pair: Pair<Rule>) -> Result<Expr, String> {
    let mut inner = pair.into_inner();
    let lhs = unquote(inner.next().ok_or_else(|| "empty comparison".to_string())?.as_str());
    match inner.next() {
        None => Ok(Expr::Atom(lhs)),
        Some(op_pair) => {
            let op = match op_pair.as_str() {
                "==" => CmpOp::Eq,
                "!=" => CmpOp::Ne,
                "<=" => CmpOp::Le,
                ">=" => CmpOp::Ge,
                "<" => CmpOp::Lt,
                ">" => CmpOp::Gt,
                other => return Err(format!("unknown operator '{other}'")),
            };
            let rhs_pair = inner.next().ok_or_else(|| "comparison missing right-hand side".to_string())?;
            Ok(Expr::Cmp(lhs, op, unquote(rhs_pair.as_str())))
        }
    }
}

/// Strips surrounding quotes and resolves backslash escapes on a quoted
/// arg. Bare (unquoted) args pass through unchanged.
fn unquote(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return raw.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(escaped) = chars.next() {
                out.push(escaped);
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Variable substitution
// ---------------------------------------------------------------------------

/// Replaces `$1`, `$2`, ... in `text` with the corresponding (1-indexed)
/// entry of `args`. An index past the end of `args` becomes an empty
/// string rather than an error, so an alias can be called with fewer
/// arguments than it references.
fn substitute(text: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let is_digit_next = matches!(chars.peek(), Some(d) if d.is_ascii_digit());
        if c == '$' && is_digit_next {
            let mut digits = String::new();
            while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
                digits.push(chars.next().unwrap());
            }
            let idx: usize = digits.parse().unwrap_or(0);
            if idx >= 1 {
                if let Some(value) = args.get(idx - 1) {
                    out.push_str(value);
                }
                continue;
            }
            out.push('$');
            out.push_str(&digits);
        } else {
            out.push(c);
        }
    }
    out
}

/// Evaluates an `if` condition against the args in scope, substituting
/// `$N` in each leaf just before comparing.
fn eval_expr(expr: &Expr, args: &[String]) -> bool {
    match expr {
        Expr::Atom(s) => {
            let v = substitute(s, args);
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Expr::Cmp(lhs, op, rhs) => {
            let l = substitute(lhs, args);
            let r = substitute(rhs, args);
            match op {
                CmpOp::Eq => l == r,
                CmpOp::Ne => l != r,
                // Numeric comparison when both sides parse as numbers,
                // otherwise fall back to lexicographic string comparison --
                // lets both `$1 < $2` (numbers) and `$1 < $2` (names) do
                // something reasonable.
                _ => match (l.parse::<f64>(), r.parse::<f64>()) {
                    (Ok(ln), Ok(rn)) => compare(ln, *op, rn),
                    _ => compare(l.as_str(), *op, r.as_str()),
                },
            }
        }
        Expr::And(l, r) => eval_expr(l, args) && eval_expr(r, args),
        Expr::Or(l, r) => eval_expr(l, args) || eval_expr(r, args),
    }
}

fn compare<T: PartialOrd>(l: T, op: CmpOp, r: T) -> bool {
    match op {
        CmpOp::Lt => l < r,
        CmpOp::Le => l <= r,
        CmpOp::Gt => l > r,
        CmpOp::Ge => l >= r,
        CmpOp::Eq | CmpOp::Ne => unreachable!("handled directly in eval_expr"),
    }
}

// ---------------------------------------------------------------------------
// Host contract
// ---------------------------------------------------------------------------

/// Side effects the interpreter can't perform itself -- implemented by
/// whatever owns the actual application state.
pub trait CommandHost {
    /// Runs any command that isn't one of the interpreter's own
    /// control-flow builtins (`alias`, `if`, `load`) and isn't a
    /// user-defined alias. This is where `connect`, `msg`, `echo`, `raw`,
    /// and "unknown command" reporting all live.
    fn run_command(&mut self, name: &str, args: &[String]);
    /// Reads a script file's contents for `/load`.
    fn read_script_file(&mut self, path: &str) -> Result<String, String>;
    /// Reports a script-level problem: bad `if` syntax, a missing alias
    /// block, an unreadable file, hitting the recursion limit, etc.
    fn report_error(&mut self, message: &str);
}

// ---------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------

/// Guards against unbounded recursion from a self-referential (or mutually
/// recursive) alias.
const MAX_DEPTH: usize = 32;

/// Holds user-defined aliases and executes parsed scripts against a
/// [`CommandHost`]. Create one and keep it around for the life of the
/// program so aliases persist across separate commands the user types over
/// time.
pub struct Interpreter {
    aliases: HashMap<String, Vec<Stmt>>,
}

impl Interpreter {
    pub fn new() -> Self {
        Self { aliases: HashMap::new() }
    }

    /// Runs a parsed script with `args` as the in-scope `$1`, `$2`, ...
    /// values (empty at top level).
    pub fn exec(&mut self, script: &Script, args: &[String], host: &mut dyn CommandHost) {
        self.exec_depth(&script.0, args, host, 0);
    }

    fn exec_depth(&mut self, stmts: &[Stmt], args: &[String], host: &mut dyn CommandHost, depth: usize) {
        if depth > MAX_DEPTH {
            host.report_error("alias recursion limit exceeded");
            return;
        }
        for stmt in stmts {
            self.exec_stmt(stmt, args, host, depth);
        }
    }

    fn exec_stmt(&mut self, stmt: &Stmt, args: &[String], host: &mut dyn CommandHost, depth: usize) {
        match stmt {
            Stmt::If { cond, block } => {
                // The if-block shares the *enclosing* scope's arguments --
                // it's conditional execution, not a new alias invocation.
                if eval_expr(cond, args) {
                    self.exec_depth(block, args, host, depth + 1);
                }
            }
            Stmt::Command { name, args: raw_args, block } => {
                let resolved: Vec<String> = raw_args.iter().map(|a| substitute(a, args)).collect();
                let lname = name.trim_start_matches('/').to_ascii_lowercase();
                match lname.as_str() {
                    "alias" => self.define_alias(&resolved, block.as_deref(), host),
                    "load" => self.exec_load(&resolved, host, depth),
                    other => {
                        if let Some(body) = self.aliases.get(other).cloned() {
                            self.exec_depth(&body, &resolved, host, depth + 1);
                        } else {
                            host.run_command(other, &resolved);
                        }
                    }
                }
            }
        }
    }

    fn define_alias(&mut self, resolved: &[String], block: Option<&[Stmt]>, host: &mut dyn CommandHost) {
        let Some(alias_name) = resolved.first() else {
            host.report_error("usage: alias <name> { ...commands... }");
            return;
        };
        let Some(block) = block else {
            host.report_error("usage: alias <name> { ...commands... }");
            return;
        };
        let alias_name = alias_name.to_ascii_lowercase();
        if matches!(alias_name.as_str(), "alias" | "if" | "load") {
            host.report_error("can't name an alias 'alias', 'if', or 'load' -- those are reserved");
            return;
        }
        self.aliases.insert(alias_name.clone(), block.to_vec());
        host.run_command("echo", &[format!("Alias '{alias_name}' defined.")]);
    }

    fn exec_load(&mut self, resolved: &[String], host: &mut dyn CommandHost, depth: usize) {
        let Some(path) = resolved.first() else {
            host.report_error("usage: load <file>");
            return;
        };
        let contents = match host.read_script_file(path) {
            Ok(c) => c,
            Err(e) => {
                host.report_error(&format!("could not read {path}: {e}"));
                return;
            }
        };
        match parse_script(&contents) {
            // A loaded script runs at top level -- no $N arguments in scope.
            Ok(script) => self.exec_depth(&script.0, &[], host, depth + 1),
            Err(e) => host.report_error(&format!("error parsing {path}: {e}")),
        }
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
