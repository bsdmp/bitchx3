//! A small scripting layer for chat commands.
//!
//! This module owns parsing (tokenizing text into a block tree) and
//! interpretation (control flow: `alias`, `if`, `load`, `$N` argument
//! substitution, recursion). It deliberately knows nothing about IRC or
//! networking -- any command it doesn't recognize as one of its own
//! control-flow builtins is handed to whatever implements [`CommandHost`],
//! which is where `connect`, `msg`, `echo`, etc. actually live (in
//! `main.rs`). That keeps this file reusable for a very different host if
//! the command set ever changes.
//!
//! Grammar, informally:
//!   script     := statement*
//!   statement  := NAME arg* block?
//!   arg        := WORD | "quoted string"
//!   block      := '{' script '}'
//!   Statements are separated by newlines, `;`, or simply follow one
//!   another (a block ends the current statement).
//!
//! A leading `/` on a command name is always stripped, so `/connect` and
//! `connect` are equivalent everywhere -- callers decide whether they
//! *require* the slash (e.g. the top-level chat input does; statements
//! inside a `{ }` block don't need to).

use std::collections::HashMap;

/// One parsed statement: a command name, its (not-yet-substituted)
/// argument tokens, and an optional trailing `{ ... }` block for commands
/// that take one (`alias`, `if`).
#[derive(Debug, Clone)]
pub struct Statement {
    pub name: String,
    pub args: Vec<String>,
    pub block: Option<Vec<Statement>>,
}

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
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    LBrace,
    RBrace,
    Terminator, // newline or `;` -- consecutive ones collapse to one
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\r' => {
                chars.next();
            }
            '\n' | ';' => {
                chars.next();
                if tokens.last() != Some(&Token::Terminator) {
                    tokens.push(Token::Terminator);
                }
            }
            '{' => {
                chars.next();
                tokens.push(Token::LBrace);
            }
            '}' => {
                chars.next();
                tokens.push(Token::RBrace);
            }
            '"' => {
                chars.next(); // opening quote
                let mut word = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(escaped) => word.push(escaped),
                            None => return Err("unterminated escape in quoted string".to_string()),
                        },
                        Some(ch) => word.push(ch),
                        None => return Err("unterminated quoted string".to_string()),
                    }
                }
                tokens.push(Token::Word(word));
            }
            _ => {
                let mut word = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_whitespace() || ch == '{' || ch == '}' || ch == ';' || ch == '"' {
                        break;
                    }
                    word.push(ch);
                    chars.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn skip_terminators(&mut self) {
        while matches!(self.peek(), Some(Token::Terminator)) {
            self.pos += 1;
        }
    }

    /// Parses statements until end-of-input or an (unconsumed) `}` that the
    /// caller is responsible for matching.
    fn parse_block(&mut self) -> Result<Vec<Statement>, String> {
        let mut stmts = Vec::new();
        loop {
            self.skip_terminators();
            match self.peek() {
                None | Some(Token::RBrace) => break,
                Some(Token::LBrace) => {
                    return Err("unexpected '{' -- a block must follow a command name".to_string());
                }
                Some(Token::Word(_)) => {
                    let name = match self.next() {
                        Some(Token::Word(w)) => w,
                        _ => unreachable!(),
                    };
                    let mut args = Vec::new();
                    let mut block = None;
                    loop {
                        match self.peek() {
                            Some(Token::Word(_)) => {
                                if let Some(Token::Word(w)) = self.next() {
                                    args.push(w);
                                }
                            }
                            Some(Token::LBrace) => {
                                self.next(); // consume '{'
                                let inner = self.parse_block()?;
                                match self.next() {
                                    Some(Token::RBrace) => {}
                                    _ => return Err("missing closing '}'".to_string()),
                                }
                                block = Some(inner);
                                break;
                            }
                            _ => break, // Terminator, RBrace, or EOF ends the statement
                        }
                    }
                    stmts.push(Statement { name, args, block });
                }
                Some(Token::Terminator) => unreachable!("skip_terminators already consumed these"),
            }
        }
        Ok(stmts)
    }
}

/// Parses a whole script -- one input line, or a whole file's contents --
/// into a sequence of statements.
pub fn parse_script(text: &str) -> Result<Vec<Statement>, String> {
    let tokens = tokenize(text)?;
    let mut parser = Parser { tokens, pos: 0 };
    let stmts = parser.parse_block()?;
    if parser.pos != parser.tokens.len() {
        return Err("unexpected '}' with no matching '{'".to_string());
    }
    Ok(stmts)
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

// ---------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------

/// Guards against unbounded recursion from a self-referential (or mutually
/// recursive) alias.
const MAX_DEPTH: usize = 32;

/// Holds user-defined aliases and executes parsed scripts against a
/// [`CommandHost`]. Create one and keep it around for the life of the
/// program so aliases persist across separate `/alias`, `/if`, and plain
/// commands the user types over time.
pub struct Interpreter {
    aliases: HashMap<String, Vec<Statement>>,
}

impl Interpreter {
    pub fn new() -> Self {
        Self { aliases: HashMap::new() }
    }

    /// Runs a parsed script with `args` as the in-scope `$1`, `$2`, ...
    /// values (empty at top level).
    pub fn exec(&mut self, stmts: &[Statement], args: &[String], host: &mut dyn CommandHost) {
        self.exec_depth(stmts, args, host, 0);
    }

    fn exec_depth(&mut self, stmts: &[Statement], args: &[String], host: &mut dyn CommandHost, depth: usize) {
        if depth > MAX_DEPTH {
            host.report_error("alias recursion limit exceeded");
            return;
        }
        for stmt in stmts {
            self.exec_statement(stmt, args, host, depth);
        }
    }

    fn exec_statement(&mut self, stmt: &Statement, args: &[String], host: &mut dyn CommandHost, depth: usize) {
        let resolved: Vec<String> = stmt.args.iter().map(|a| substitute(a, args)).collect();
        let name = stmt.name.trim_start_matches('/').to_ascii_lowercase();

        match name.as_str() {
            "alias" => self.define_alias(&resolved, stmt.block.as_deref(), host),
            "if" => self.exec_if(&resolved, stmt.block.as_deref(), args, host, depth),
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

    fn define_alias(&mut self, resolved: &[String], block: Option<&[Statement]>, host: &mut dyn CommandHost) {
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

    fn exec_if(
        &mut self,
        resolved: &[String],
        block: Option<&[Statement]>,
        outer_args: &[String],
        host: &mut dyn CommandHost,
        depth: usize,
    ) {
        let Some(block) = block else {
            host.report_error("usage: if <a> == <b> { ...commands... }");
            return;
        };
        if resolved.len() != 3 {
            host.report_error("usage: if <a> == <b> { ...commands... }");
            return;
        }
        let (lhs, op, rhs) = (&resolved[0], resolved[1].as_str(), &resolved[2]);
        let cond = match op {
            "==" => lhs == rhs,
            "!=" => lhs != rhs,
            other => {
                host.report_error(&format!("unsupported operator '{other}' (only == and != are supported)"));
                return;
            }
        };
        if cond {
            // The if-block shares the *enclosing* scope's arguments -- it's
            // conditional execution, not a new alias invocation.
            self.exec_depth(block, outer_args, host, depth + 1);
        }
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
            Ok(stmts) => self.exec_depth(&stmts, &[], host, depth + 1),
            Err(e) => host.report_error(&format!("error parsing {path}: {e}")),
        }
    }
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}
