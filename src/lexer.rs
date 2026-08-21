use crate::is_debug;
use crate::executor::escape;

use logos::{Logos, Lexer, SpannedIter};
use std::collections::VecDeque;
use std::ops::Range;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone)]
pub struct LexerState<'a> { //re-initialize to new instance on every lex of cmd_buf
    pub cmd_buf: &'a str,

    //for heredocs
    delimiters: VecDeque<String>, //tracking heredoc delimiters must be String type not &str,
                                  //because a quoted heredoc delimiter or a delimiter with escapes
                                  //requires creation of a new String to match the delimiter in body
    heredocs: Vec<(usize, usize)>, //(doc_start, doc_end)

    subsh_matched: bool,
    pub syntax_err: Option<String>,
    pub expected_closer: Option<String>,
    pub continuation_for: Option<String>, //if cmd ends with &&, ||, |, or \, need to prompt user
}

impl<'a> LexerState<'a> {
    pub fn new(cmd_buf: &'a str) -> Self {
        Self {
            cmd_buf: cmd_buf,
            delimiters: VecDeque::new(),
            heredocs: Vec::new(),
            subsh_matched: false,
            syntax_err: None,
            expected_closer: None,
            continuation_for: None,
        }
    }
}

#[derive(Logos, Debug, PartialEq, Clone, Serialize, Deserialize)]
#[logos(extras = LexerState<'s>)]
pub enum Tkn {
    #[regex(r#"[^ `"'\\\t\f\n|&;<>(){}=$]+"#)] // bare literal, use span to get text
    Literal,

    #[token("<", redirect_callback)]
    RedirectIn,

    #[token(">", redirect_callback)]
    RedirectOut,

    #[token(">>", redirect_callback)]
    RedirectAppend,

    #[token("<<", heredoc_callback)]
    Heredoc,

    #[token("|")] 
    Pipe,

    #[token("\\")]
    Backslash,

    #[token("&&")]
    CmdAnd,

    #[token("||")]
    CmdOr,

    #[token(";")]
    Semicolon,

    #[token("&")]
    And,

    #[token("=", priority = 2)]
    Assign,

    #[token("\"", double_quote_callback)]
    DoubleQuote(Vec<TknSpan>),

    #[token("'", single_quote_callback)]
    SingleQuote, // Single quotes do no expansion

    #[token("`", backtick_callback)]
    #[token("$", eval_callback)]
    Expansion, // Captures variables and command substitutions

    #[token("(", subsh_callback)]
    LParen,

    #[token(")")]
    RParen,

    #[token("\n", newline_handler)]
    Newline,

    #[regex(r#"[ \t\f]+"#)]
    Space,

    Word(Vec<TknSpan>),
}

impl fmt::Display for Tkn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tkn::Literal => "literal",
            Tkn::RedirectIn => "<",
            Tkn::RedirectOut => ">",
            Tkn::RedirectAppend => ">>",
            Tkn::Heredoc => "<<",
            Tkn::Pipe => "|",
            Tkn::Backslash => "\\",
            Tkn::CmdAnd => "&&",
            Tkn::CmdOr => "||",
            Tkn::Semicolon => ";",
            Tkn::And => "&",
            Tkn::Assign => "=",
            Tkn::DoubleQuote(_) => "double quote",
            Tkn::SingleQuote => "single quote",
            Tkn::Expansion => "expansion",
            Tkn::LParen => "(",
            Tkn::RParen => ")",
            Tkn::Newline => "newline",
            Tkn::Space => "space",
            Tkn::Word(_) => "word",
        };
        write!(f, "{}", s)
    }
}

// ----------------------------------------------------------------------------
// Word Parts Lexing Callbacks
// ----------------------------------------------------------------------------

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(extras = LexerState<'s>)]
enum DQuoteTkn {
    #[regex(r#"[^"\\$`]+"#)] Text,
    #[token("\\")] Escape, // Captures escape sequences inside quotes
    #[token("$")] Dollar,
    #[token("`")] Backtick,
    #[token("\"")] QuoteEnd,
}

fn double_quote_callback<'a, T>(lex: &mut Lexer<'a, T>) -> Option<Vec<TknSpan>> 
where T: Logos<'a, Extras = LexerState<'a>, Source = str> + Clone {
    let mut parts = Vec::new();
    let mut dq_lex = lex.clone().morph::<DQuoteTkn>();

    while let Some(res) = dq_lex.next() {
        match res {
            Ok(DQuoteTkn::Text) => {
                parts.push(TknSpan{ kind: Tkn::Literal, span: dq_lex.span() });
            }
            Ok(DQuoteTkn::Escape) => {
                let start = dq_lex.span().start;
                let mut end = dq_lex.span().end;
                
                // Bump the lexer past the escaped char so it doesn't trigger Dollar/QuoteEnd
                if let Some(c) = dq_lex.remainder().chars().next() {
                    dq_lex.bump(c.len_utf8());
                    end += c.len_utf8();
                }
                parts.push(TknSpan { kind: Tkn::Literal, span: start..end });
            }
            Ok(DQuoteTkn::Dollar) => {
                let start = dq_lex.span().start;
                if !parse_expansion(dq_lex.remainder(), &mut dq_lex) {
                    return None
                } 
                let end = dq_lex.span().end;
                parts.push(TknSpan { kind: Tkn::Expansion, span: start..end });
            }
            Ok(DQuoteTkn::Backtick) => {
                let start = dq_lex.span().start;
                let remainder = dq_lex.remainder();
                let mut end_idx = 0;
                let mut in_escape = false;
                let mut found = false;
                
                for (i, c) in remainder.char_indices() {
                    if in_escape { in_escape = false; continue; }
                    if c == '\\' { in_escape = true; } 
                    else if c == '`' {
                        end_idx = i;
                        found = true;
                        break;
                    }
                }
                
                if found {
                    let end = start + 1 + end_idx + 1;
                    dq_lex.bump(end_idx + 1);
                    parts.push(TknSpan{ kind: Tkn::Expansion, span: start..end });
                } else {
                    lex.extras.expected_closer = Some("`".to_string());
                    return None;
                }
            }
            Ok(DQuoteTkn::QuoteEnd) => {
                let num_read_bytes = lex.remainder().len() - dq_lex.remainder().len();
                lex.bump(num_read_bytes);
                lex.extras = dq_lex.extras;
                return Some(parts);
            }
            Err(_) => {
                lex.extras.syntax_err = Some("invalid token inside double quote".to_string());
                return None;
            }
        }
    }
    lex.extras.expected_closer = Some("\"".to_string());
    None
}

fn single_quote_callback<'a, T>(lex: &mut Lexer<'a, T>) -> bool 
where T: Logos<'a, Extras = LexerState<'a>, Source = str> + Clone {
    let remainder = lex.remainder();
    if let Some(end_idx) = remainder.find('\'') {
        lex.bump(end_idx + 1); // +1 to consume closing quote
        return true
    }
    lex.extras.expected_closer = Some("\'".to_string());
    return false
}

fn backtick_callback<'a, T>(lex: &mut Lexer<'a, T>) -> bool 
where T: Logos<'a, Extras = LexerState<'a>, Source = str> + Clone {
    let mut in_escape = false;
    let remainder = lex.remainder();
    for (i, c) in remainder.char_indices() {
        if in_escape {
            in_escape = false;
            continue;
        }
        if c == '\\' {
            in_escape = true;
        } else if c == '`' {
            lex.bump(i + 1);
            return true;
        }
    }
    lex.extras.expected_closer = Some("`".to_string());
    false
}

fn eval_callback<'a, T>(lex: &mut Lexer<'a, T>) -> bool 
where T: Logos<'a, Extras = LexerState<'a>, Source = str> + Clone {
    parse_expansion(lex.remainder(), lex)
}

/// Helper function to parse `${var}`, `$var`, or `$(cmd)` 
/// Returns the number of string bytes consumed (excluding the initial `$`).
fn parse_expansion<'a, T>(remainder: &str, lex: &mut Lexer<'a, T>) -> bool
where T: Logos<'a, Extras = LexerState<'a>, Source = str> + Clone {
    let mut chars = remainder.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return true, // naked '$' at EOF
    };

    match first {
        '{' => {
            if let Some(end_idx) = remainder.find('}') {
                lex.bump(end_idx + 1);
                return true;
            } else {
                lex.extras.expected_closer = Some("}".to_string());
                return false;
            }
        }
        '(' => {
            if chars.next().is_none() { 
                lex.extras.expected_closer = Some("$)".to_string());
                return false;
            }
            if let Some(bytes_consumed) = parse_parentheses(&remainder[1..]) {
                //starting from index 1 is safe because of the chars check above
                lex.bump(bytes_consumed + 1);
                return true;
            }
            lex.extras.expected_closer = Some("$)".to_string());
            return false;
        }
        c if c.is_alphabetic() || c == '_' => {
            let mut bytes_consumed = c.len_utf8();
            for c in chars {
                if c.is_alphanumeric() || c == '_' {
                    bytes_consumed += c.len_utf8();
                } else {
                    break;
                }
            }
            lex.bump(bytes_consumed); 
            return true;
        }
        '?' | '*' | '@' | '#' | '$' | '0'..='9' => {
            lex.bump(first.len_utf8());
            return true;
        }
        _ => {
            return true;
            // Un-expandable sequence, treat as literal "$"
        }
    }
}

// ----------------------------------------------------------------------------
// TargetDelim & Callbacks
// ----------------------------------------------------------------------------

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(extras = LexerState<'s>)]
#[logos(skip r"[ \t\f]+")]
enum TargetDelim { 
    #[token("\n")]
    Newline,

    #[regex(r#"[^ `"'\t\n\f|&;<>(){}$]+"#)]
    Delim,

    #[token("\"", double_quote_callback)]
    DoubleQuote(Vec<TknSpan>),

    #[token("'", single_quote_callback)]
    SingleQuote,

    #[token("`", backtick_callback)]
    #[token("$", eval_callback)]
    Expansion,
}

fn redirect_callback(lex: &mut Lexer<Tkn>) -> bool {
    let mut delim_lex = lex.clone().morph::<TargetDelim>();
    let operator = delim_lex.slice();
    let mut success = false;
    match delim_lex.next() {
        Some(Ok(TargetDelim::Delim) | 
             Ok(TargetDelim::DoubleQuote(_)) | 
             Ok(TargetDelim::SingleQuote) | 
             Ok(TargetDelim::Expansion)) => {
            success = true;
        },
        _ => {
            delim_lex.extras.syntax_err = Some(format!("not a valid delimiter for {}", operator));
        }
    }
    lex.extras = delim_lex.extras;
    success
}

fn heredoc_callback(lex: &mut Lexer<Tkn>) -> bool {
    let mut delim_lex = lex.clone().morph::<TargetDelim>();
    let mut success = false;
    match delim_lex.next() {
        Some(Ok(TargetDelim::Delim) | 
            Ok(TargetDelim::Expansion)) => {
            delim_lex.extras.delimiters.push_back(delim_lex.slice().to_string());
            success = true;
        },
        Some(Ok(TargetDelim::DoubleQuote(word_parts))) => {
            //"fix this so that it doesn't evaluate expansions but escapes Literals; need cmd buf reference"
            let mut delim = String::new();
            for p in word_parts.iter() {
                match &p.kind {
                    Tkn::Literal => delim.push_str(&escape(get_token_at(p, delim_lex.extras.cmd_buf))),
                    Tkn::Expansion => delim.push_str(get_token_at(p, delim_lex.extras.cmd_buf)),
                    _ => return false, //impossible, double_quote_callback guarantees one or the other
                }
            }
            delim_lex.extras.delimiters.push_back(delim);
            success = true;
        },
        Some(Ok(TargetDelim::SingleQuote)) => {
            let slice = delim_lex.slice();
            delim_lex.extras.delimiters.push_back(slice[1..slice.len()-1].to_string());
            success = true;
        },
        _ => {
            delim_lex.extras.syntax_err = Some("not a valid delimiter for <<".to_string());
        },
    }
    lex.extras = delim_lex.extras;
    success                                        
}

fn subsh_callback(lex: &mut Lexer<Tkn>) -> bool {
    if lex.extras.subsh_matched { 
        return true; 
    } else {
        lex.extras.subsh_matched = parse_parentheses(lex.remainder()).is_some();
    }
    lex.extras.subsh_matched
}

fn parse_parentheses<'a>(remainder: &'a str) -> Option<usize> {
    let mut depth: usize = 1;
    let mut bytes_consumed = 0;
    for c in remainder.chars() {
        bytes_consumed += c.len_utf8();
        if c == '(' { depth += 1; }
        if c == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(bytes_consumed);
            }
        }
    }
    None
} 

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(extras = LexerState<'s>)]
enum HeredocTkn {
    #[regex(r#"[^\n]*\n"#, allow_greedy = true)]
    HeredocLine,
}

fn newline_handler(lex: &mut Lexer<Tkn>) -> bool { 
    let mut heredoc_start = lex.span().end;
    let mut heredoc_end = lex.span().end;
    let mut heredoc_lex = lex.clone().morph::<HeredocTkn>();
    let mut line_len = 0;

    while let Some(delim) = heredoc_lex.extras.delimiters.pop_front() {
        let mut closed = false;
        while let Some(res) = heredoc_lex.next() {
            match res {
                Ok(HeredocTkn::HeredocLine) => {
                    let line = heredoc_lex.slice();
                    line_len = line.len();
                    if line.trim_end() == &delim {
                        closed = true;
                        break;
                    }
                    heredoc_end += line_len;
                },
                Err(e) => panic!("ERR: {:?}", e),
            }
        }
        if closed {
            heredoc_lex.extras.heredocs.push((heredoc_start, heredoc_end));
            heredoc_end += line_len;
            heredoc_start = heredoc_end;
        } else { 
            heredoc_lex.extras.expected_closer = Some(delim);
            *lex = heredoc_lex.morph();
            return false;        
        }
    }

    let num_read_bytes = lex.remainder().len() - heredoc_lex.remainder().len();
    lex.bump(num_read_bytes); 

    lex.extras = heredoc_lex.extras;
    true
}


#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TknSpan {
    pub kind: Tkn,
    pub span: Range<usize>,
}

pub fn lex_cmd_buf<'a> (span_iter: &mut SpannedIter<'a, Tkn>, cmd_buf: &'a str) -> Option<(Vec<TknSpan>, VecDeque<&'a str>)> {
    let mut tkns: Vec<TknSpan> = Vec::new();
    let mut word_parts = Vec::new();
    let is_debug = is_debug();
    if is_debug { println!("DEBUG OUTPUT!"); }
    
    for (res, span) in &mut *span_iter {
        match res {
            Ok(tkn) => {
                if is_debug {
                    match tkn {
                        Tkn::Newline => println!(),
                        Tkn::Space => (),
                        _ => print!("tkn: '{}'; ", &cmd_buf[span.start..span.end]),
                    }
                }
                match tkn {
                    // Embed DoubleQuote along with its inner components so the execution layer knows it was quoted.
                    Tkn::Literal | Tkn::SingleQuote | Tkn::Expansion | Tkn::DoubleQuote(_) => {
                        word_parts.push(TknSpan{ kind: tkn, span });
                    },
                    _ => {
                        if !word_parts.is_empty() {
                            let start = word_parts.first().unwrap().span.start;
                            let end = word_parts.last().unwrap().span.end;
                            tkns.push(TknSpan {
                                kind: Tkn::Word(std::mem::take(&mut word_parts)), 
                                span: start..end,
                            }); 
                        }
                        //push the non-word tkn span as well
                        tkns.push(TknSpan {kind: tkn, span});
                    }
                }
            },
            Err(_) => return None,
        }
    }

    // Capture the final word if the command string didn't end in space or newline
    if !word_parts.is_empty() {
        let start = word_parts.first().unwrap().span.start;
        let end = word_parts.last().unwrap().span.end;
        tkns.push(TknSpan {
            kind: Tkn::Word(std::mem::take(&mut word_parts)),
            span: start..end,
        });
    }

    for tkn in tkns.iter().rev() {
        match tkn.kind {
            Tkn::Space | Tkn::Newline => continue,
            Tkn::Pipe | Tkn::CmdOr | Tkn::CmdAnd => {
                span_iter.extras.continuation_for = Some(get_token_at(tkn, cmd_buf).to_string());
                return None;
            }
            _ => break,
        }
    }
    
    let mut heredocs = VecDeque::with_capacity(span_iter.extras.heredocs.len());
    for (doc_start, doc_end) in span_iter.extras.heredocs.iter() {
        let heredoc = &cmd_buf[*doc_start..*doc_end];
        heredocs.push_back(heredoc);
    }
    Some((tkns, heredocs))
}

pub fn get_token_at<'a>(tkn_span: &'a TknSpan, cmd_buf: &'a (impl AsRef<str> + ?Sized)) -> &'a str {
    let slice_ref = cmd_buf.as_ref();
    &slice_ref[tkn_span.span.start..tkn_span.span.end]
}
