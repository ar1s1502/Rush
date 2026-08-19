use crate::lexer::{LexerState, Tkn, lex_cmd_buf, get_token_at };
use logos::Logos;
use anyhow::anyhow;

#[allow(dead_code)]
fn run_cmd(cmd_buf: &str, args: &[&str]) -> anyhow::Result<()> {
    let lex_state = LexerState::new(cmd_buf);
    let mut lex = Tkn::lexer_with_extras(&cmd_buf, lex_state).spanned();
    match lex_cmd_buf(&mut lex, &cmd_buf) {
        Some((tkns, _)) => {
            for (i, tkn) in tkns.iter().enumerate() {
                println!("{}", get_token_at(tkn, cmd_buf));
                assert_eq!(&cmd_buf[tkn.span.start..tkn.span.end], args[i]); 
            }
            Ok(())
        }
        None => return Err(anyhow!("lex fail: {}", cmd_buf)),
    }
}

#[test]
fn test_lexer() -> anyhow::Result<()> {
    // 1. simple test
    let tkns = vec!["echo", " ", "hello", " ", "world", " ", "\n"];
    let cmd_buf = "echo hello world \n".to_string();
    run_cmd(&cmd_buf, &tkns)?;

    // 2. mixed test with heredoc
    let tkns = vec![
        "asdf", " ", "ASDFAW", " ", "1234125", " ", "###", " ", "<", " ", "iipo", " ",
        ">>", " ", "98767", " ", "<<", " ", "\"ghj\"", " ", ">", " ", "'6789'", " ", "\nghj\n",
    ];
    let cmd_buf = "asdf ASDFAW 1234125 ### < iipo >> 98767 << \"ghj\" > '6789' \nghj\n".to_string();
    run_cmd(&cmd_buf, &tkns)?;

    // 3. string literal chaos test
    let tkns = vec!["\"678987678987 asdfas > < >> << || {} :: '' / \\ @#%\"", "\n"];
    let cmd_buf = tkns.join("");
    run_cmd(&cmd_buf, &tkns)?;

    // 4. Multiple sequential heredocs (cat << A << B << C)
    let tkns = vec![
        "cat", " ", "<<", " ", "A", " ", "<<", " ", "B", " ", "<<", " ", "C", " ",
        "\ncontentA\nA\ncontentB\nB\ncontentC\nC\n",
    ];
    let cmd_buf = "cat << A << B << C \ncontentA\nA\ncontentB\nB\ncontentC\nC\n".to_string();
    run_cmd(&cmd_buf, &tkns)?;

    // 5. Multiple heredocs separated by pipeline stages (cat << A | cat << B)
    let tkns = vec![
        "cat", " ", "<<", " ", "A", " ", "|", " ", "cat", " ", "<<", " ", "B", " ",
        "\nbodyA\nA\nbodyB\nB\n",
    ];
    let cmd_buf = "cat << A | cat << B \nbodyA\nA\nbodyB\nB\n".to_string();
    run_cmd(&cmd_buf, &tkns)?;

    // 6. space mixup - No spaces between operator and delimiter (<<EOF)
    let tkns = vec!["cat", " ", "<<", "EOF", "\nhello\nEOF\n"];
    let cmd_buf = "cat <<EOF\nhello\nEOF\n".to_string();
    run_cmd(&cmd_buf, &tkns)?;

    // 7. space mixup - Multi-space padding before delimiter (<<   EOF)
    let tkns = vec!["cat", " ", "<<", "   ", "EOF", "\nhello spacey heredoc\nEOF\n"];
    let cmd_buf = "cat <<   EOF\nhello spacey heredoc\nEOF\n".to_string();
    run_cmd(&cmd_buf, &tkns)?;

    // 8. space mixup - operators squished with commands
    let tkns = vec![
        "cat", " ", "Cargo.toml", "|", "grep", " ", "c", "||", "tail", " ", "-n", " ",
        "3", "&&", "echo", " ", "hello", "\n",
    ];
    let cmd_buf = "cat Cargo.toml|grep c||tail -n 3&&echo hello\n".to_string();
    run_cmd(&cmd_buf, &tkns)?;

    Ok(())
}

#[allow(dead_code)]
fn run_cmd_should_fail(cmd_buf: &str) -> bool {
    let lex_state = LexerState::new(cmd_buf);
    let mut lex = Tkn::lexer_with_extras(&cmd_buf, lex_state).spanned();
    
    // Returns true if the lexer successfully caught the error and returned None
    lex_cmd_buf(&mut lex, &cmd_buf).is_none()
}

#[test]
fn test_lexer_fails() {
    // 1. Unclosed parenthesis
    assert!(run_cmd_should_fail("( \n"));
    assert!(run_cmd_should_fail("echo hello ( cat file \n"));

    // 2. Heredoc with no EOF delimiter token before the newline
    assert!(run_cmd_should_fail("cat << \n"));
    assert!(run_cmd_should_fail("cat <<      \n"));

    // 3. Redirects (<, >, >>) missing a target file token
    assert!(run_cmd_should_fail("cat < \n"));
    assert!(run_cmd_should_fail("echo hello > \n"));
    assert!(run_cmd_should_fail("echo goodbye >> \n"));
    assert!(run_cmd_should_fail("cat < | grep 'test' \n")); // Target missing before pipe

    // 4. Trailing pipeline or logical operators (2nd to last token violations)
    assert!(run_cmd_should_fail("cat file | \n"));
    assert!(run_cmd_should_fail("echo 'done' || \n"));
    assert!(run_cmd_should_fail("cargo build && \n"));
    assert!(run_cmd_should_fail("ls -la |   \n")); // With trailing spaces
}

