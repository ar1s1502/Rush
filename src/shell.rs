mod lexer;
mod parser;
mod executor;

#[cfg(test)]
mod unit_tests;

use lexer::{Tkn, TknSpan, lex_cmd_buf, LexerState};
use executor::{execute_cmd_buf, execute_ast};
use logos::{Logos, };
use rustyline::{DefaultEditor};
use rustyline::error::ReadlineError;
use std::{env, process};
use std::io::{Write, self, IsTerminal};
use std::collections::{VecDeque, HashMap};
use std::cell::{RefCell};
use std::time::Duration;
use std::thread;

const HISTORY_PATH: &str = "rust_shell_history.txt"; 
pub const AS_SUBSHELL: &str = "--as-subshell";

struct ShellState {
    pub debug: bool, //run in debug mode flag
    pub tty: bool,   //connected to terminal?
    pub user: String,
    pub devicename: String,
}

thread_local! { //create these in thread local memory so that it can be accessed like mutable globals across modules
    pub static RL_EDITOR: RefCell<DefaultEditor> = RefCell::new(DefaultEditor::new().expect("failed to initiate default editor"));

    pub static SHELL_STATE: RefCell<ShellState> = RefCell::new(ShellState {
        debug: false,
        tty: io::stdout().is_terminal() && io::stdin().is_terminal(),
        user: whoami::account().unwrap_or("<unknown>".to_string()),
        devicename: whoami::devicename().unwrap_or("<unknown>".to_string()).replace(" ","-"),
    });

    pub static ENV_VARS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

pub fn set_debug(debug: bool) {
    SHELL_STATE.with_borrow_mut(|s| s.debug = debug);
}
pub fn is_debug() -> bool {
    SHELL_STATE.with_borrow(|s| s.debug) 
}
fn is_tty() -> bool {
    SHELL_STATE.with_borrow(|s| s.tty)
}
pub fn get_env_var(key: &str) -> String {
    ENV_VARS.with_borrow(|ev| 
        ev.get(key).map_or("".to_string(), |val| val.clone())
    )
}
pub fn put_env_var(key: String, val: String) -> Option<String> {
    ENV_VARS.with_borrow_mut(|ev| ev.insert(key, val))
}

/* 
OSC Escape sequence's data so that frontend typescript can differentiate between shell outputs
OSC 133 syntax: \x1b]133;${data}\x07
see https://contour-terminal.org/vt-extensions/osc-133-shell-integration/ 
*/
const PROMPT_START: &str = "A";
const PROMPT_END: &str = "B";
const CMD_OUTPUT_START: &str = "C";
const CMD_END: &str = "D";

//only if '--debug'/'-d' flag set
fn print_cmd<'a> (tkns: &'a [TknSpan], heredocs: &VecDeque<&'a str>) {
    for tkn in tkns.iter() {
        println!("{:?}, {:?}", tkn.kind, tkn.span);
    }
    if !heredocs.is_empty() {
        println!("heredocs: {:?}", heredocs);
    }
}

fn parse_flags() {
    for (i, arg) in env::args().enumerate() {
        if i == 0 { continue; }//skip the program name 
        match arg.as_ref() {
            "-d" | "--debug" => SHELL_STATE.with_borrow_mut(|s| s.debug = true),
            _ => {
                eprintln!("unsupported flag {}", arg);
                process::exit(1);
            },
        }
    }
}

fn main() -> rustyline::Result<()> {
    //check if this process is running as a subshell
    //TODO: add cryptography to ensure that the subshell really spawned by shell
    if let Ok(serialized_ast) = env::var(AS_SUBSHELL) {
        let parsed_ast = serde_json::from_str(&serialized_ast).unwrap_or_else(|_| {
            process::exit(1);
        });
        parse_flags();
        match execute_ast(parsed_ast) {
            Ok(exit_code) => process::exit(exit_code),
            Err(_) => process::exit(1),
        }
    }
    
    /* Flags */
    parse_flags();

    //this is running in the foreground, do REPL 
    send_osc133(CMD_OUTPUT_START);
    load_history()?;
    send_osc133(CMD_END);
    let mut cmd_buf = String::new();
    let mut prompt = String::new();
    set_normal_prompt(&mut prompt,);
    loop {
        //sleep fixes a weird race condition where sometimes the promptline prints before the output
        //on the cat << A | cat << B | cat << C test case
        thread::sleep(Duration::from_millis(1_5)); 
        let readline_res = RL_EDITOR.with_borrow_mut(|rl| {
            rl.readline(&prompt)
        });
        match readline_res {
            Ok(input) => {
                if input.trim().is_empty() { continue; }
                cmd_buf.push_str(&input);
                cmd_buf.push('\n'); //add back newline that readline stripped after user hit Enter
                let mut lex = Tkn::lexer_with_extras(&cmd_buf, LexerState::new(&cmd_buf)).spanned();
                match lex_cmd_buf(&mut lex, &cmd_buf) {
                    Some((mut tkns, heredocs)) => {
                        send_osc133(PROMPT_END);
                        send_osc133(CMD_OUTPUT_START);
                        if is_debug() { print_cmd(&tkns, &heredocs); }
                        if let Err(e) = execute_cmd_buf(&cmd_buf, &mut tkns, heredocs) {
                            eprintln!("{}", e);
                        }
                        send_osc133(CMD_END);
                        set_normal_prompt(&mut prompt,);
                        //Add to history
                        let _ = RL_EDITOR.with_borrow_mut(|rl| {
                            rl.add_history_entry(cmd_buf.trim())
                        });
                        cmd_buf.clear();
                    },
                    None => {
                        if let Some(ref err) = lex.extras.syntax_err {
                            send_osc133(PROMPT_END);
                            //syntax errs get highest priority b/c they're unrecoverable
                            send_osc133(CMD_OUTPUT_START);
                            println!("Syntax ERR: {}", err);
                            send_osc133(CMD_END);
                            set_normal_prompt(&mut prompt,);
                            let _ = RL_EDITOR.with_borrow_mut(|rl| {
                                rl.add_history_entry(cmd_buf.trim())
                            });
                            cmd_buf.clear();
                        } else if let Some(ref closer) = lex.extras.expected_closer {
                            set_expected_closer_prompt(&mut prompt, closer);
                        } else if let Some(ref op) = lex.extras.continuation_for {
                            set_needs_continuation_prompt(&mut prompt, op);
                        }
                    }
                }
            },
            Err(ReadlineError::Interrupted) => {
                if is_tty() {
                    send_osc133(PROMPT_END);
                    println!("CTRL-C");
                }
                set_normal_prompt(&mut prompt,);
                cmd_buf.clear();
            },
            Err(ReadlineError::Eof) => {
                if is_tty() {
                    send_osc133(PROMPT_END);
                    println!("CTRL-D");
                }
                let _ = exit_shell(&[""], None);
            },
            Err(err) => {
                send_osc133(PROMPT_END);
                eprintln!("ERR: {:?}", err);
                let _ = exit_shell(&[""], None);
            },
        }
    }
}

fn set_normal_prompt(prompt: &mut String) { 
    let cwd = env::current_dir().unwrap().file_name().unwrap().to_str().unwrap().to_string();
    SHELL_STATE.with_borrow(|s| {
        *prompt = format!("[rust_shell] {}@{}: {} % ", &s.user, &s.devicename, cwd);
    });
    send_osc133(PROMPT_START);
}

//handles cmd lines that end with \, &&, |, ||
fn set_needs_continuation_prompt(prompt: &mut String, op: &str) {
    match op {
        "\\" => *prompt = String::from("> "),
        "&&" => *prompt = String::from("CmdAnd> "), 
        "||" => *prompt = String::from("CmdOr> "),
        "|" => *prompt = String::from("pipe> "),
        _ => *prompt = String::from("> "),
    }
    send_osc133(&format!("{};{}", PROMPT_START, prompt));
}

//handles unclosed quoted strings, heredocs with no closing delimiters
fn set_expected_closer_prompt(prompt: &mut String, closer: &str) {
    match closer {
        "\'" => *prompt = String::from("quote> "),
        "`" => *prompt = String::from("bquote> "),
        "\"" => *prompt = String::from("dquote> "),
        ")" => *prompt = String::from("subsh> "),
        "$)" => *prompt = String::from("cmdsubst> "),
        _ => *prompt = format!("missing {} for heredoc> ", closer),
    }
    send_osc133(&format!("{};{}", PROMPT_START, prompt))
} 

fn send_osc133(data: &str) {
    if is_tty() {
        print!("\x1b]133;{}\x07", data);
        //flush because stdout might buffer until newline
        let _ = std::io::stdout().flush();
    }
}

fn save_history() -> anyhow::Result<String> {
    let save_path = env::home_dir().unwrap().join(HISTORY_PATH);
    RL_EDITOR.with_borrow_mut(|rl| {
        rl.save_history(&save_path)
    })?;
    Ok(format!("{}", save_path.display()))
}

fn load_history() -> rustyline::Result<()> {
    let save_path = env::home_dir().unwrap().join(HISTORY_PATH);
    RL_EDITOR.with_borrow_mut(|rl| {
        rl.load_history(&save_path)
    })
}

pub fn exit_shell(_args: &[&str], _pipe_reader: Option<std::io::PipeReader>) -> anyhow::Result<String> {
    if is_tty() {
        send_osc133(CMD_OUTPUT_START);
        println!("exiting shell...");
    }
    match save_history() {
        Ok(save_path) => if is_tty() { println!("shell command history saved to {}", save_path); },
        Err(e) => eprintln!("ERR: {}", e),
    }
    process::exit(0);
}
