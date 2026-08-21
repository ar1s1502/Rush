use crate::lexer::{TknSpan, Tkn, get_token_at};
use crate::executor::{ChildPr, Builtin, Subsh, Redirect, Redir, Assignment, Word, eval_word, get_builtins };
use std::collections::VecDeque;
use std::borrow::Cow;
use std::iter::{Peekable,};
use std::slice::IterMut;
use serde::{Deserialize, Serialize};
use anyhow::anyhow;

/* 
 * Recursive Descent Parser
 * See https://ruslanspivak.com/lsbasi-part7/ for an e.g.
 * */

#[derive(Serialize, Deserialize)]
pub enum AstNode<'a> {
    #[serde(borrow)]
    Prog(ChildPr<'a>),

    Builtin(Builtin<'a>),

    Logical {
        lhs: Box<AstNode<'a>>,
        operator: Tkn,
        rhs: Box<AstNode<'a>>,
    },

    Pipeline(Vec<Box<AstNode<'a>>>),

    Subshell(Subsh<'a>),

    Assignments {
        assignments: Vec<Assignment>, 
        cmd_buf: Cow<'a, str>,
    }, //global shell-scoped env vars
}

pub struct Parser<'a>
{
    tkns: Peekable<IterMut<'a, TknSpan>>,
    cur_tkn: Option<&'a mut TknSpan>,
    heredocs: VecDeque<&'a str>,
    cmd_buf: &'a str,
}

impl<'a> Parser<'a> {
    pub fn new(tkns_: &'a mut [TknSpan], heredocs: VecDeque<&'a str>, cmd_buf: &'a str) -> Self {
        Self {
            tkns: tkns_.iter_mut().peekable(),
            cur_tkn: None,
            heredocs,
            cmd_buf
        }
    }

    fn advance(&mut self) -> Option<&mut TknSpan> {
        self.cur_tkn = self.tkns.next();
        self.cur_tkn.as_deref_mut()
    }
    
    fn eat(&mut self, expected_type: Tkn) -> anyhow::Result<()> {
        if let Some(tkn) = self.advance() {
            if tkn.kind != expected_type {
                return Err(anyhow!("Syntax err: expected token of kind {} but got '{}'", expected_type, tkn.kind));
            }
        } else {
            return Err(anyhow!("Syntax err: missing token {:?}", expected_type));
        }
        Ok(())
    }

    //goal is to create an AstNode::Assignment which the executor evaluates at runtime
    //an assignment has a lhs (a word), and a rhs (an AstNode syntax tree)
    //in the match statement in the while loop, accept words, quotes, l/rparen. anything else throw
    //syntax error. stop at the same program delims, except also include whitespace
    fn assign_to(&mut self, lhs: Word) -> anyhow::Result<Assignment> {
        self.eat(Tkn::Assign)?; //consume the '=' tkn
        let mut rhs = Vec::new();
        while let Some(tkn) = self.tkns.peek() {
            match &tkn.kind {
                Tkn::Word(_) => {
                    if let Some(Tkn::Word(word_parts)) = self.advance().map(|tknspan| &mut tknspan.kind) {
                        rhs = std::mem::take(word_parts);
                    }
                }
                Tkn::Space => {
                    if !rhs.is_empty() {
                        return Ok(Assignment{lhs, rhs}); //cloning cmd_buf pointer is cheap
                    }
                    self.eat(Tkn::Space)?;
                }
                Tkn::Newline | Tkn::Semicolon | Tkn::CmdOr | Tkn::CmdAnd | Tkn::RParen | Tkn::Pipe => {
                    return Ok(Assignment{lhs, rhs});  //cloning cmd buf pointer is cheap
                }
                _ => {
                    anyhow::bail!("Syntax ERR: {} is an invalid token for assignment", get_token_at(tkn, self.cmd_buf));
                }
            }
        }
        anyhow::bail!("unreachable");
    }

    fn expr(&mut self) -> anyhow::Result<Box<AstNode<'a>>> {
        let mut redirect_ins = Vec::new();
        let mut redirect_outs = Vec::new();
        let mut args: Vec<Word> = Vec::new(); // a Word is a Vec<TknSpan>
        let mut inner_ast_ = None;
        let mut env_vars = Vec::new();
        while let Some(cur_tkn) = self.advance() {
            match &mut cur_tkn.kind {
                Tkn::Word(word_parts) => {
                    //check if the next non-whitespace token is an '='
                    let word: Word = std::mem::take(word_parts);
                    if let Some(Tkn::Assign) = self.tkns.peek().map(|t| &t.kind) {
                        env_vars.push(self.assign_to(word)?);
                        continue
                    } else if let Some(Tkn::Space) = self.tkns.peek().map(|t| &t.kind) {
                        self.eat(Tkn::Space)?;
                        if let Some(Tkn::Assign) = self.tkns.peek().map(|t| &t.kind) {
                            env_vars.push(self.assign_to(word)?);
                            continue
                        }
                    }
                    //else next isn't an '=', this must be a program arg
                    args.push(word);
                }
                Tkn::Assign => {  //should be covered in Tkn::Word arm
                    anyhow::bail!("Syntax Err: invalid assignment");
                }
                /* redirects */ 
                Tkn::RedirectIn => {
                    if self.tkns.peek().map_or(false, |t| t.kind == Tkn::Space) { self.eat(Tkn::Space)?; }
                    if let Some(Tkn::Word(parts)) = self.advance().map(|tkn| &mut tkn.kind) {
                        let infile = std::mem::take(parts);
                        redirect_ins.push(Redirect { dir: Redir::In, file: infile, heredoc_file: None});
                    } else {
                        anyhow::bail!("Couldn't find infile after >");
                    }
                },
                Tkn::RedirectOut => {
                    if self.tkns.peek().map_or(false, |t| t.kind == Tkn::Space) { self.eat(Tkn::Space)?; }
                    if let Some(Tkn::Word(parts)) = self.advance().map(|tkn| &mut tkn.kind) {
                        let outfile = std::mem::take(parts);
                        redirect_outs.push(Redirect { dir: Redir::Out, file: outfile, heredoc_file: None });
                    } else {
                        anyhow::bail!("Couldn't find outfile after >");
                    }
                },
                Tkn::RedirectAppend => {
                    if self.tkns.peek().map_or(false, |t| t.kind == Tkn::Space) { self.eat(Tkn::Space)?; }
                    if let Some(Tkn::Word(parts)) = self.advance().map(|tkn| &mut tkn.kind) {
                        let outfile = std::mem::take(parts);
                        redirect_outs.push(Redirect { dir: Redir::Append, file: outfile, heredoc_file: None});
                    } else {
                        anyhow::bail!("Couldn't find outfile after >>");
                    }
                },
                Tkn::Heredoc => {
                    //must create owned copy of heredoc, because it later must cross thread boundary
                    let heredoc_content = self.heredocs.pop_front().unwrap_or("").to_string();
                    redirect_ins.push(Redirect { dir: Redir::Heredoc, file: vec![], heredoc_file: Some(heredoc_content) });
                    if self.tkns.peek().map_or(false, |t| t.kind == Tkn::Space) { self.eat(Tkn::Space)?; }
                    //eat the heredoc delimiter
                    match self.tkns.peek().map(|t| &t.kind) {
                        Some(Tkn::Word(_)) => { 
                            self.advance();
                        }
                        Some(_) => {
                            anyhow::bail!("unreachable: Invalid delimiter for heredoc");
                        }
                        None => {
                            anyhow::bail!("unreachable: Expected heredoc delimiter, found EOF");
                        }
                    };
                },
                /* Grouped commands in parentheses => spawn a subshell */ 
                Tkn::LParen => {
                    if !args.is_empty() { anyhow::bail!("Syntax Err: found '{}...' before subshell start", get_token_at(&args[0][0], self.cmd_buf)); }
                    inner_ast_ = Some(self.build_subshell()?);
                }
                /* program delimiters */
                Tkn::Newline | Tkn::Semicolon | Tkn::CmdOr | Tkn::CmdAnd | Tkn::RParen | Tkn::Pipe => {
                    break;
                },
                Tkn::Space => (), //ignore spaces
                _ => return Err(anyhow!("Syntax Err: unexpected tkn in expression")),
            }
        }
        if args.is_empty() && inner_ast_.is_none() && env_vars.is_empty() { 
            return Err(anyhow!("Syntax Err: empty args"));
        }
        if inner_ast_.is_some() && !env_vars.is_empty() {
            anyhow::bail!("Syntax Err: inline assignment outside of subshell");
        }
        //if inner_ast is some, then we built a subshell program
        if let Some(inner_ast) = inner_ast_ {
            if !env_vars.is_empty() {
                anyhow::bail!("Syntax Err: invalid assignment");
            }
            return Ok(Box::new(AstNode::Subshell(Subsh {
                cmd_buf: Cow::Borrowed(self.cmd_buf),
                inner_ast,
                redirect_ins,
                redirect_outs,
            })));
        }
        //if assignments, but no args, then put the assignments in global shell ENV_VARS map 
        if !env_vars.is_empty() && args.is_empty() {
            return Ok(Box::new(AstNode::Assignments{ 
                assignments: env_vars,
                cmd_buf: Cow::Borrowed(self.cmd_buf),
            }));
        }
        //if args[0] is a builtin command, then return astnode::builtin
        if get_builtins().get(eval_word(&args[0], self.cmd_buf).as_str()).is_some() {
            return Ok(Box::new(AstNode::Builtin(Builtin {
                args,
                cmd_buf: Cow::Borrowed(self.cmd_buf), //cheap b/c just cloning the pointer 
                redirect_ins,
                redirect_outs,
            })));
        }
        //else we have to look up in $PATH
        return Ok(Box::new(AstNode::Prog(ChildPr {
            args,
            cmd_buf: Cow::Borrowed(self.cmd_buf),
            redirect_ins,
            redirect_outs,
            env_vars,
        })));
    }

    fn build_subshell(&mut self) -> anyhow::Result<Vec<Box<AstNode<'a>>>> {
        let mut subsh = Vec::new();
        while self.cur_tkn.as_ref().map_or(false, |t| t.kind != Tkn::RParen) {
            subsh.push(self.build_ast()?);
            //build ast stops at a newline, semicolon, or rparen
            if self.cur_tkn.as_ref().map_or(false, |t| t.kind == Tkn::RParen) { break; }
            self.ignore_next_program_delims();
            if self.tkns.peek().map_or(false, |t| t.kind == Tkn::RParen) { 
                //found the closing paren for subshell
                self.eat(Tkn::RParen)?;
            }
        }
        Ok(subsh)
    }

    fn build_pipeline(&mut self) -> anyhow::Result<Box<AstNode<'a>>> {
        let mut node = self.expr()?;
        if self.cur_tkn.as_ref().map_or(false, |tkn| tkn.kind == Tkn::Pipe) {
            let mut pipeline = vec![node];
            while let Some(tkn) = &self.cur_tkn {
                if tkn.kind != Tkn::Pipe { break; }
                self.ignore_next_program_delims();
                node = self.expr()?;
                pipeline.push(node);
            }
            return Ok(Box::new(AstNode::Pipeline(pipeline)))
        }
        Ok(node)
    }

    fn build_ast(&mut self) -> anyhow::Result<Box<AstNode<'a>>> {
        let mut node = self.build_pipeline()?;
        loop {
            if self.cur_tkn.is_none() {
                //this shouldn't be reachable but just in case
                return Err(anyhow!("Syntax Err"));
            }
            let tkn_kind = self.cur_tkn.as_ref().unwrap().kind.clone(); 
            match tkn_kind {
                Tkn::Newline | Tkn::Semicolon | Tkn::RParen => return Ok(node),
                Tkn::CmdOr | Tkn::CmdAnd => {
                    self.ignore_next_program_delims();
                    node = Box::new(AstNode::Logical {
                        lhs: node,
                        operator: tkn_kind,
                        rhs: self.build_pipeline()?,
                    });
                },
                _ => return Err(anyhow!("Syntax Err: expected \\n, ;, ||, or && but got '{}'", tkn_kind)),
            }
        }
    }

    pub fn parse(&mut self) -> anyhow::Result<Vec<Box<AstNode<'a>>>> {
        let mut executables = Vec::new();
        self.ignore_next_program_delims();
        while self.tkns.peek().is_some() {
            let node = self.build_ast()?;
            if self.cur_tkn.is_none() {
                //this shouldn't be reachable but just in case
                return Err(anyhow!("Syntax Err"));
            }
            let tkn = self.cur_tkn.as_ref().unwrap();
            match tkn.kind {
                Tkn::Newline | Tkn::Semicolon | Tkn::RParen => {
                    executables.push(node);
                    self.ignore_next_program_delims();
                }
                _ => return Err(anyhow!("Syntax Err:\nwhile parsing, expected '\\n', ';', or ')', but got '{}'", tkn.kind)),
            }
        }
        Ok(executables)
    }

    fn ignore_next_program_delims(&mut self) {
        while let Some(tkn) = self.tkns.peek() {
            if [Tkn::Newline, Tkn::Semicolon, Tkn::Space].contains(&tkn.kind) {
                self.advance();
            } else { break; }
        }
    }
}



