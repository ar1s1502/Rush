use crate::is_debug;
use crate::executor::escape;

use logos::{Logos, Lexer, SpannedIter };
use std::collections::VecDeque;
use std::ops::Range;
use serde::{Deserialize, Serialize};
use std::fmt;

///TODO: Custom error handling for lexer

#[derive(Debug, Clone)]
pub struct LexerState { //re-initialize to new instance on every lex of cmd_buf
    //for heredocs
    delimiters: VecDeque<String>,
    heredocs: Vec<(usize, usize)>, //(doc_start, doc_end)

    pub syntax_err: Option<String>,
    pub bracket_closers: VecDeque<char>,
    pub expected_closer: Option<String>,
    pub continuation_for: Option<String>, //if cmd ends with &&, ||, |, or \, need to prompt user
}

impl LexerState {
    pub fn new() -> Self {
        Self {
            delimiters: VecDeque::new(),
            heredocs: Vec::new(),
            syntax_err: None,
            bracket_closers: VecDeque::new(),
            expected_closer: None,
            continuation_for: None,
        }
    }
}

#[derive(Logos, Debug, PartialEq, Clone, Serialize, Deserialize)]
#[logos(extras = LexerState)]
pub enum Tkn {
    #[regex(r#"[^ `"'\\\t\f\n|&;<>(){}=$]+"#)]
    Word,

    #[token("<", redirect_callback)]
    RedirectIn,

    #[token(">", redirect_callback)]
    RedirectOut,

    #[token(">>", redirect_callback)]
    RedirectAppend,

    #[token("<<", heredoc_callback)]
    Heredoc,

    #[token("|", )] 
    Pipe,

    #[token("\\", )]
    Backslash,

    #[token("&&", )]
    CmdAnd,

    #[token("||", )]
    CmdOr,

    #[token(";")]
    Semicolon,

    #[token("&")]
    And,

    #[regex(r#"[`'"]"#, quote_handler)]
    Quote,

    #[token("=")]
    Assign,

    #[token("$")]
    Eval,

    #[token("(", bracket_callback)]
    LParen,

    #[token(")", bracket_callback)]
    RParen,

    #[token("\n", newline_handler)]
    Newline,

    #[regex(r#"[ \t\f]+"#)]
    Space,
}

//not used for tkn generation, only for debugging/error reporting
//use str reference to cmd buf for tkns
impl fmt::Display for Tkn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tkn::Word => "word",
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
            Tkn::Quote => "quoted string",
            Tkn::LParen => "(",
            Tkn::RParen => ")",
            Tkn::Newline => "newline",
            Tkn::Assign => "=",
            Tkn::Eval => "$",
            Tkn::Space => "Space",
        };
        write!(f, "{}", s)
    }
}

/*
 * NOTE: if a logos callback function returns a Option/Result/bool and 
 * the callback None/Err/false is returned, then the lex.next() call that triggers the callback will be Some(Err(_))
 */

fn redirect_callback(lex: &mut Lexer<Tkn>) -> bool {
    let mut delim_lex = lex.clone().morph::<TargetDelim>();
    let operator = delim_lex.slice();
    let mut success = false;
    //look ahead to see if the next token is a valid filename
    //does not advance lex iterator. i.e. if delim_lex finds valid filename,
    //it will be consumed as a Tkn::Word in the next lext.next() call
    match delim_lex.next() {
        Some(Ok(TargetDelim::Delim(_)) | Ok(TargetDelim::Quote)) => {
            //found a valid filename
            success = true;
        },
        _ => {
            delim_lex.extras.syntax_err = Some(format!("not a valid delimiter for {}", operator));
        }
    }

    lex.extras = delim_lex.extras; //match LexerStates
    success
}

//handles |, ||, &&, and 
fn operator_callback(lex: &mut Lexer<Tkn>) -> Option<()> {
    let mut delim_lex = lex.clone().morph::<TargetDelim>();
    let operator = delim_lex.slice();
    //look ahead to see if the next token is a valid delimiter
    //|, ||, && can not be followed by another operator. if newline, then must prompt for
    //continuation
    //does not advance lex iterator. i.e. if delim_lex finds valid delimiter,
    //it will be consumed as a Tkn::Word in the next lext.next() call
    match delim_lex.next() {
        Some(Ok(TargetDelim::Delim(_)) | Ok(TargetDelim::Quote)) => {
            delim_lex.extras.continuation_for = None;
        },
        Some(Ok(TargetDelim::Newline)) => {
            delim_lex.extras.continuation_for = Some(operator.to_string());
        },
        _ => { //invalid input following operator, like another shell operator, (), etc.
            delim_lex.extras.syntax_err = Some(format!("parse error near {}", delim_lex.span().end));
        },
    }
    lex.extras = delim_lex.extras; //match LexerStates
    Some(())
}

fn heredoc_callback(lex: &mut Lexer<Tkn>) -> bool {
    let mut delim_lex = lex.clone().morph::<TargetDelim>();
    let mut success = false;
    //look ahead to see if the next token is a valid heredoc delimiter
    //does not advance lex iterator. i.e. if delim_lex finds valid delimiter,
    //it will be consumed as a Tkn::Word in the next lext.next() call
    match delim_lex.next() {
        Some(Ok(TargetDelim::Delim(delim))) => {
            delim_lex.extras.delimiters.push_back(delim);
            success = true;
        }
        Some(Ok(TargetDelim::Quote)) => {
            let delim = escape(delim_lex.slice());
            delim_lex.extras.delimiters.push_back(delim);
            success = true;
        }
        _ => {
            delim_lex.extras.syntax_err = Some("not a valid delimiter for <<".to_string());
        },

    }
    lex.extras = delim_lex.extras; //match LexerStates
    success                                   
}

fn bracket_callback(lex: &mut Lexer<Tkn>) -> bool {
    let bracket = lex.slice();
    match bracket {
        "(" => lex.extras.bracket_closers.push_front(')'),
        "[" => lex.extras.bracket_closers.push_front(']'),
        "{" => lex.extras.bracket_closers.push_front('}'),
        ")" | "]" | "}" => {
            if let Some(closer) = lex.extras.bracket_closers.front() {
                if bracket.chars().nth(0).unwrap() == *closer {
                    lex.extras.bracket_closers.pop_front();
                    return true;
                }
            } 
            return false;
        }
        _ => (),
    }
    return true;
}

//returns a VecDeque of heredocs (if any) to be handed to the parser
fn newline_handler(lex: &mut Lexer<Tkn>) -> bool { 
    let mut heredoc_start = lex.span().end; //heredoc (if any) starts right after the newline
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
        } else { //we have to poll for more input from shell
            heredoc_lex.extras.expected_closer = Some(delim);
            *lex = heredoc_lex.morph();
            return false;        
        }
    }

    //set the span of Tkn lexer to match the whole heredoc content
    let num_read_bytes = lex.remainder().len() - heredoc_lex.remainder().len();
    lex.bump(num_read_bytes); 

    lex.extras = heredoc_lex.extras;
    true
}

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(extras = LexerState)]
enum QuoteTkn {
    #[regex(r#"['"`]"#)] // Match any potential closer
    PotentialCloser,
    
    #[token("\\")]
    Escape,

    #[regex(r#"[^'"`\\]"#)]
    //stop matching Text at a backslash, cuz backslash in quotes must escape next char
    Char,
}

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(extras = LexerState)]
#[logos(skip r"[ \t\f]+")] // Ignore this regex pattern between token
enum TargetDelim { //for finding valid target after one of <, >, <<, >>, ||, &&, or =
    // A valid delimiter is 1 or more characters that are NOT whitespace or shell operators.
    #[regex(r#"['"`]"#, quote_handler)]
    Quote,

    #[token("\n")]
    Newline,

    #[regex(r#"[^ `"'\t\n\f|&;<>(){}]+"#, |lex| lex.slice().to_string())]
    Delim(String),
}

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(extras = LexerState)]
enum HeredocTkn {
    //match any number of characters, ended with a newline
    #[regex(r#"[^\n]*\n"#, allow_greedy = true)]
    HeredocLine,
}

// #[derive(Logos, Debug, PartialEq, Clone)]
// #[logos(extras = LexerState)]
// enum AssignmentTkn {
//     //match any number of characters, ended with a newline
//     #[regex(r#"[^]*[\n; ]"#, allow_greedy = true)]
//     HeredocLine,
// }

fn quote_handler<'a, T>(lex: &mut Lexer<'a, T>) -> bool 
where T: Logos<'a, Extras = LexerState, Source = str> + Clone {
    assert!(lex.extras.expected_closer.is_none());
    let mut quote_lex = lex.clone().morph::<QuoteTkn>();
    //closing quote must match the opening quote
    quote_lex.extras.expected_closer = Some(quote_lex.slice().to_string());
    while let Some(res) = quote_lex.next() {
        match res {
            Ok(QuoteTkn::PotentialCloser) => {
                let quote = quote_lex.slice();
                if Some(quote.to_string()) == quote_lex.extras.expected_closer {
                    quote_lex.extras.expected_closer = None;
                    break;
                }
            },
            Ok(QuoteTkn::Char) => (),
            Ok(QuoteTkn::Escape) => {
                if quote_lex.extras.expected_closer != Some("\'".to_string()) {
                    //backslash doesn't escape chars in single quoted strings
                    _ = quote_lex.next();
                }
            }
            Err(e) => panic!("ERR: {:?}", e),
        }
    } 
    
    //set the span of Tkn lexer to match the whole quoted string content
    let num_read_bytes = lex.remainder().len() - quote_lex.remainder().len();
    lex.bump(num_read_bytes); 

    lex.extras = quote_lex.extras; //sync states
    lex.extras.expected_closer.is_none() 
}

pub struct TknSpan {
    pub kind: Tkn,
    pub span: Range<usize>,
}

pub fn lex_cmd_buf<'a> (span_iter: &mut SpannedIter<'a, Tkn>, cmd_buf: &'a str) -> Option<(Vec<TknSpan>, VecDeque<&'a str>)> {
    let mut tkns: Vec<TknSpan> = Vec::new();
    //fresh borrow of span_iter so can get span_iter.extras later
    for (res, span) in &mut *span_iter {
        match res {
            Ok(tkn) => {
                if is_debug() {
                    match tkn {
                        Tkn::Newline => println!(),
                        _ => print!("tkn: '{}'; ", &cmd_buf[span.start..span.end]),
                    }
                }
                tkns.push(TknSpan {kind: tkn, span});
            },
            Err(_) => {
                return None;
            }
        }
    }

    if !span_iter.extras.bracket_closers.is_empty() { return None; } //unclosed paren, bracket, or curly

    //check if last non-whitespace (not space or newline) tkn is an operator 
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

pub fn get_token_at<'a>(tkn_span: &'a TknSpan, cmd_buf: &'a str) -> &'a str {
    // if let Tkn::Quote(ref quote_content) = tkn_span.kind {
    //     return quote_content;
    // }
    &cmd_buf[tkn_span.span.start..tkn_span.span.end]
}


