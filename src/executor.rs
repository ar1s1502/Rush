use crate::parser::{AstNode, Parser};
use crate::lexer::{TknSpan, Tkn};
use crate::{AS_SUBSHELL, RL_EDITOR, is_debug, exit_shell, put_env_var, get_env_var};
use serde::{Deserialize, Serialize};
use std::collections::{VecDeque, HashMap};
use std::process::{self, Command, ExitStatus, Stdio, };
use std::borrow::Cow;
use std::fs::{File, OpenOptions, };
use std::io::{Write, self, PipeReader, PipeWriter, Read};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::mem;
use rustyline::history::{History, SearchDirection};

type BuiltinFn = fn(&[&str], Option<PipeReader>) -> anyhow::Result<String>;

//Global immutable hashmap of <builtin command name>:<function to execute builtin>
pub static BUILTINS: OnceLock<HashMap<&'static str, BuiltinFn>> = OnceLock::new();
pub fn get_builtins() -> &'static HashMap<&'static str, BuiltinFn> {
    BUILTINS.get_or_init(|| {
        HashMap::from([
            ("pwd", pwd as BuiltinFn),
            ("cd", set_cwd),
            ("history", get_history),
            ("exit", exit_shell),
        ])
    })
}

#[derive(Serialize, Deserialize, )]
pub enum Redir {
    In, //<
    Out,//>
    Append, //>>
    Heredoc,//<<
}

#[derive(Serialize, Deserialize, )]
pub struct Redirect {
    pub dir: Redir,
    pub file: String,
}

#[derive(Serialize, Deserialize, )]
pub struct Assignment<'a> {
    pub lhs: &'a str, 
    pub rhs: Vec<&'a str>,
    pub delim_kind: Tkn, //which tkn kind finished the rhs
}

impl<'a> Assignment<'a> {
    pub fn evaluate_rhs(&self) -> String {
        let mut res = String::new();
        for &expr in self.rhs.iter() {
            if needs_escape(expr) {
                res.push_str(&escape(expr));
            } else {
                res.push_str(expr);
            }
        }
        println!("res: {}", res);
        println!("delim_kind: {}", self.delim_kind);
        res
    }
}

#[derive(Serialize, Deserialize)]
pub struct Builtin<'a> { //built in command 
    #[serde(borrow)]
    pub args: Vec<Cow<'a, str>>,
    // I/O streams for redirection
    pub redirect_ins: Vec<Redirect>,
    pub redirect_outs: Vec<Redirect>,
}

impl<'a> Builtin<'a> {
    pub fn exec_builtin(&mut self, pipe_write: Option<PipeWriter>, pipe_read: Option<PipeReader>)
    -> anyhow::Result<()> {
        expand_quotes(&mut self.args);
        let cleaned_asref: Vec<&str> = self.args.iter().map(|arg| arg.as_ref()).collect();
        let builtin_fn = get_builtins().get(cleaned_asref[0]).unwrap(); //unwrap safe because parser checks if in builtins
        match builtin_fn(&cleaned_asref, pipe_read) {
            Ok(output_str) => {
                if !self.redirect_outs.is_empty() {
                    let redirects = mem::take(&mut self.redirect_outs);
                    use std::io::Cursor;
                    write_to_redirect_outs(redirects, Cursor::new(output_str))?;
                } else if let Some(mut pipe_writer) = pipe_write {
                    thread::spawn(move || {
                        let _ = pipe_writer.write_all(output_str.as_bytes());
                    });
                } else {
                    println!("{}", output_str);
                }
            },
            Err(e) => eprintln!("{}", e),
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct ChildPr<'a> { //a child process spawned by shell
    #[serde(borrow)]
    pub args: Vec<Cow<'a, str>>,
    // I/O streams for redirection
    pub redirect_ins: Vec<Redirect>,
    pub redirect_outs: Vec<Redirect>,
    //assignments
    pub env_vars: Vec<Assignment<'a>>,
}

impl<'a> ChildPr<'a> {
    pub fn spawn(&mut self, mut stdin: Stdio, mut stdout: Stdio) 
    -> anyhow::Result<process::Child> {
        if !self.redirect_ins.is_empty() { stdin = Stdio::piped(); }
        if !self.redirect_outs.is_empty() { stdout = Stdio::piped(); }
        
        expand_quotes(&mut self.args);
        let cleaned_asref: Vec<&str> = self.args.iter().map(|arg| arg.as_ref()).collect();
        let mut handle = Command::new(cleaned_asref[0]);
        if cleaned_asref.len() > 1 {
            handle.args(&cleaned_asref[1..]); 
        }
        handle.stdin(stdin);
        handle.stdout(stdout);
        self.apply_env_vars(&mut handle);
        let mut c = handle.spawn()?;
        self.apply_redirect(&mut c)?;
        Ok(c)
    }

    fn apply_env_vars(&self, handle: &mut Command) {
        for assignment in self.env_vars.iter() {
            handle.env(assignment.lhs, assignment.evaluate_rhs());
        }
    }

    //same as spawn, but will wait for process to finish and collect status
    pub fn status(&mut self) -> anyhow::Result<ExitStatus> {
        let mut c = self.spawn(Stdio::inherit(), Stdio::inherit())?;
        Ok(c.wait()?)
    }

    //apply any redirect operators (<, <<, >, >>)
    fn apply_redirect(&mut self, c: &mut process::Child) -> anyhow::Result<()> {
        if !self.redirect_ins.is_empty() {
            let mut stdin_handle = c.stdin.take().expect("Failed to take child stdin handle");
            let redirects = mem::take(&mut self.redirect_ins);
            thread::spawn(move || {
                for r in redirects.into_iter() {
                    match r.dir {
                        Redir::Heredoc => { 
                            //Redirect.file is heredoc content in this case, not file path
                            let _ = stdin_handle.write_all((&r.file).as_bytes());
                        },
                        Redir::In => {
                            if !Path::new(&r.file).is_file() { anyhow::bail!("ERR: {} is not a valid file", r.file); }
                            if let Ok(mut f) = File::open(&r.file) {
                                //write to child stdin in chunks until f's eof
                                let _ = std::io::copy(&mut f, &mut stdin_handle);
                            }
                        }
                        _ => (),
                    }
                }
                Ok(())
            });
        }
        if !self.redirect_outs.is_empty() {
            let stdout_handle = c.stdout.take().expect("Failed to take child stdout handle");
            let redirects = mem::take(&mut self.redirect_outs);
            write_to_redirect_outs(redirects, stdout_handle)?;
        }

        Ok(()) 
    }

}

#[derive(Serialize, Deserialize)]
pub struct Subsh<'a> {
    #[serde(borrow)]
    pub inner_ast: Vec<Box<AstNode<'a>>>,

    pub redirect_ins: Vec<Redirect>,
    pub redirect_outs: Vec<Redirect>,
}

impl<'a> Subsh<'a> {
    pub fn spawn(&mut self, mut stdin: Stdio, mut stdout: Stdio) -> anyhow::Result<process::Child> {
        let mut inner_ast = mem::take(&mut self.inner_ast);
        //File I/O redirects override inherited or pipe I/O handles
        if !self.redirect_ins.is_empty() { 
            stdin = Stdio::piped();  //override
            let redirects = mem::take(&mut self.redirect_ins);
            let first_node = &mut inner_ast[0];
            self.apply_redirect_in(redirects, first_node)?;
        }
        if !self.redirect_outs.is_empty() { stdout = Stdio::piped(); }

        let shell_exe = std::env::current_exe()?;
        let serialized_ast = serde_json::to_string(&inner_ast)?;
        let mut subsh = Command::new(shell_exe)
            .env(AS_SUBSHELL, &serialized_ast)
            .stdin(stdin)
            .stdout(stdout)
            .spawn()?;
        if !self.redirect_outs.is_empty() {
            let stdout_handle = subsh.stdout.take().expect("failed to take child process stdout");
            let redirects = mem::take(&mut self.redirect_outs);
            write_to_redirect_outs(redirects, stdout_handle)?;
        }
        Ok(subsh)
    }

    pub fn status(&mut self) -> anyhow::Result<ExitStatus> {
        let mut c = self.spawn(Stdio::inherit(), Stdio::inherit())?;
        Ok(c.wait()?)
    }

    fn apply_redirect_in (&self, redirects: Vec<Redirect>, first_node: &mut Box<AstNode<'a>>) -> anyhow::Result<()> {
        match &mut **first_node {
            AstNode::Subshell(subsh) => subsh.redirect_ins.extend(redirects),
            AstNode::Prog(child_pr) => child_pr.redirect_ins.extend(redirects),
            AstNode::Builtin(builtin) => builtin.redirect_ins.extend(redirects),
            AstNode::Logical{ lhs,..} => self.apply_redirect_in(redirects, lhs)?,
            AstNode::Pipeline(pipeline) => self.apply_redirect_in(redirects, &mut pipeline[0])?,
            AstNode::Assignments{ .. } => (), //assignment expressions don't do i/o
        }
        Ok(())
    }

}

fn write_to_redirect_outs<T>(redirects: Vec<Redirect>, mut stdout_handle: T) -> anyhow::Result<()> 
where 
    T: Read + std::marker::Send + 'static, //Send and static required because moving across thread bound
{
    let mut outfiles = Vec::new();
    //create/open all outfiles. (per Bourne shell, this happens even if command stdout is never written to)
    for r in redirects.iter() {
        match r.dir {
            Redir::Out => outfiles.push(OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&r.file)?), //This should be equivalent to File::create
            Redir::Append => outfiles.push(OpenOptions::new()
                .create(true)
                .append(true)
                .open(&r.file)?),
            _ => anyhow::bail!("unreachable: got redirect in while executing redirect out"),
        }
    }
    thread::spawn(move || {
        let mut buf = [0u8; 5*(1<<10)];
        loop {
            match stdout_handle.read(&mut buf) {
                Ok(0) => break, //EOF
                Ok(n) => {
                    for f in outfiles.iter_mut() {
                        let _ = f.write_all(&buf[..n]);
                    }
                },
                Err(_) => break, 
            }
        }
    });            
    Ok(())
}

//convert std::io::PipeWriter/Reader to std::process::Stdio
fn convert_pipe_fds(pipe_w: Option<PipeWriter>, pipe_r: Option<PipeReader>) -> (Stdio, Stdio) {
    (
        match pipe_w {
            None => Stdio::inherit(),
            Some(w_fd) => Stdio::from(w_fd),
        },
        match pipe_r {
            None => Stdio::inherit(),
            Some(r_fd) => Stdio::from(r_fd),
        }
    )
}

fn spawn_pipeline(progs: &mut Vec<Box<AstNode>>) -> anyhow::Result<Vec<process::Child>> {
    let num_prog = progs.len();
    let mut children = Vec::with_capacity(num_prog);
    let mut cur_pipe_read: Option<PipeReader> = None;
    for (i, prog) in progs.iter_mut().enumerate() {
        let last_child = i == num_prog - 1;
        let (next_pipe_read, cur_pipe_write) = 
            if last_child {
                (None, None)
            } else {
                let (pipe_reader, pipe_writer) = io::pipe()?;
                (Some(pipe_reader), Some(pipe_writer))
            };
        //prog is either a Prog(child_pr) or a Subshell(subsh)
        match &mut **prog {
            AstNode::Builtin(builtin) => {
                builtin.exec_builtin(cur_pipe_write, cur_pipe_read)?;
            },
            AstNode::Prog(child_pr) => {
                let (c_stdout, c_stdin) = convert_pipe_fds(cur_pipe_write, cur_pipe_read);
                children.push(child_pr.spawn(c_stdin, c_stdout)?);
            },
            AstNode::Subshell(subsh) => {
                let (c_stdout, c_stdin) = convert_pipe_fds(cur_pipe_write, cur_pipe_read);
                children.push(subsh.spawn(c_stdin, c_stdout)?);
            },
            //generally, a lone assignment within a pipeline doesn't affect shell state, 
            //e.g. foo=BAR | cat $foo => foo is still undefined
            //but can change this maybe?
            AstNode::Assignments(..) => (), 
            _ => anyhow::bail!("unreachable, pipe can only have Prog or Subshell"),
        }
        cur_pipe_read = next_pipe_read;
    }
    Ok(children)
}

// return the exit code of executing the ast node
fn dfs(node: &mut Box<AstNode>) -> anyhow::Result<i32> {
    match &mut **node {
        AstNode::Builtin(builtin) => {
            builtin.exec_builtin(None, None)?;
            return Ok(0);
        }
        AstNode::Prog(child_pr) => {
            if child_pr.args.is_empty() { return Ok(0); }
            return Ok(child_pr.status()?
                .code()
                .unwrap_or(1));
        },
        AstNode::Logical { 
            lhs, 
            operator, 
            rhs 
        } => {
            let lhs_code = dfs(lhs)?;
            match operator {
                Tkn::CmdOr => {
                    if lhs_code != 0 { 
                        return dfs(rhs);
                    } 
                    return Ok(0);
                },
                Tkn::CmdAnd => {
                    if lhs_code == 0 {
                        return dfs(rhs);
                    } 
                    return Ok(lhs_code);
                },
                _ => anyhow::bail!("unreachable; invalid op in Logical astnode"),
            }
        },
        AstNode::Pipeline(pipeline) => {
            let mut spawned_children = spawn_pipeline(pipeline)?;
            if spawned_children.is_empty() { return Ok(0); }
            let last = spawned_children.len() - 1;
            for (i, c) in spawned_children.iter_mut().enumerate() {
                if i == last {
                    if let Ok(exit_stat) = c.wait() {
                        return Ok(exit_stat.code().unwrap_or(1));
                    }
                    return Ok(1)
                } else {
                    let _ = c.wait();
                }
            }
            return Ok(0);
        },
        AstNode::Subshell(subshell) => {
            return Ok(subshell.status()?
                .code()
                .unwrap_or(1));
        },
        AstNode::Assignments(env_vars) => {
            for assignment in env_vars {
                put_env_var(assignment.lhs.to_string(), assignment.evaluate_rhs());
            }
            Ok(0)
        }
    }
}

pub fn execute_cmd_buf<'w> (cmd_buf: &'w str, tkns: &'w [TknSpan], heredocs: VecDeque<&'w str>) -> anyhow::Result<i32> {
    let executables = Parser::new(tkns, heredocs, cmd_buf).parse()?;
    if is_debug() { println!("\nOUTPUT!!"); }
    execute_ast(executables)
}

pub fn execute_ast(mut executables: Vec<Box<AstNode>>) -> anyhow::Result<i32> {
    let mut res = 0;
    for ast in executables.iter_mut() {
        res = dfs(ast)?;
    }
    Ok(res)
}

//evaluate quoted strings in args
fn expand_quotes<'a>(args: &mut Vec<Cow<'a, str>>) {
    for i in 0..args.len() {
        let arg = args[i].as_ref();
        if needs_escape(arg) {
            args[i] = Cow::Owned(escape(arg));
        } 
    }
}

pub fn needs_escape(arg: &str) -> bool { 
    arg.starts_with("\"") || arg.starts_with("'") 
}

//evaluates a quoted string
pub fn escape(arg: &str) -> String {
    if arg.len() < 2 {
        return arg.to_string();
    }

    //get opening quote (' or " or `)
    let quote = arg.chars().nth(0).unwrap();
    //lexer guarantees that all quoted strings are properly formatted, so this should be safe
    let inner_str = &arg[1..arg.len()-1];
    // Single quotes are literal; no escaping happens inside them.
    if quote == '\'' {
        return inner_str.to_string();
    }

    // Double quotes require escape processing.
    let mut owned = String::with_capacity(inner_str.len());
    let mut in_escape = false;
    for c in inner_str.chars() {
        if in_escape {
            match c {
                'n' => owned.push('\n'),
                't' => owned.push('\t'),
                'r' => owned.push('\r'),
                '\\' => owned.push('\\'),
                '"' => owned.push('"'),
                '`' => owned.push('`'),
                '$' => owned.push('$'),
                // POSIX Shell Rule: If a backslash precedes a character that 
                // doesn't have a special meaning, both the backslash and the character are preserved.
                _ => {
                    owned.push('\\');
                    owned.push(c);
                }
            }
            in_escape = false;
        } else if c == '\\' {
            in_escape = true;
        } else {
            owned.push(c);
        }
    }

    // If the string ended with an unclosed backslash, preserve it
    if in_escape {
        owned.push('\\');
    }
    owned
}

/* BUILTINS */
fn pwd(_args: &[&str], _pipe_reader: Option<PipeReader>) -> anyhow::Result<String> { 
    Ok(format!("{}", env::current_dir().unwrap().display()))
}

fn set_cwd(args: &[&str], _pipe_reader: Option<PipeReader>) -> anyhow::Result<String> {
    if args.len() == 1 { //cd 
        let home = env::home_dir().expect("ERR cd: Couldn't find HOME directory");
        env::set_current_dir(&home)?;
        return Ok("".to_string());
    } else if args.len() == 2 { //cd [..]
        let path_str = args[1];
        let mut new_cwd = PathBuf::from(Path::new(path_str));
        if new_cwd.starts_with("~") {
            new_cwd = expand_tilde(&new_cwd);
        }
        env::set_current_dir(&new_cwd)?;
        return Ok("".to_string());
    } else if args.len() > 2 {
        anyhow::bail!("ERR cd: too many arguments for cd; {} is invalid", args[2]);
    }
    anyhow::bail!("unreachable");
} 

//TODO: fix
fn expand_tilde(path: &PathBuf) -> PathBuf {
    let mut expanded_path = env::home_dir().expect("Couldn't find HOME directory");
    if path == Path::new("~") {
        expanded_path
    } else {
        expanded_path.push(path.strip_prefix("~").unwrap());
        expanded_path
    }
}

fn get_history(args: &[&str], _pipe_reader: Option<PipeReader>) -> anyhow::Result<String> {
    let mut output = String::new();
    if args.len() > 1 { 
        match args[1].to_lowercase().as_ref() {
            "clear" => {
                let success = RL_EDITOR.with_borrow_mut(|rl| { rl.history_mut().clear() }).is_ok();
                if success { return Ok("command history cleared".to_string()) } else { anyhow::bail!("Failed to clear history"); }
            }
            _ => anyhow::bail!("unrecognized history parameter {}", args[1]),
        };
    }
    let hist_len = RL_EDITOR.with_borrow(|h| h.history().len()) as i32;
    let start = std::cmp::max(0, hist_len - 15);
    for i in start..hist_len {
        RL_EDITOR.with_borrow(|rl| {
            let entry = rl.history().get(i as usize, SearchDirection::Forward).unwrap().unwrap().entry;
            if i != hist_len - 1 {
                output.push_str(&format!("{}\n", entry));
            } else {
                output.push_str(&entry);
            }
        })
    }
    Ok(output)
}

