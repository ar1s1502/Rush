#![allow(dead_code)]

use std::process::{Command, Stdio};
use std::io::Write;
use std::fs::{self, remove_file, File};
use std::str;

const SHELL_EXE: &'static str = env!("CARGO_BIN_EXE_rust_shell");
const GREEN: &'static str = "\x1b[32m";
const CYAN: &'static str = "\x1b[36m";
const NC: &'static str = "\x1b[0m";

fn trim_debug_output(output: &str) -> (String, String) {
    let mut debug_accum = String::new();
    let mut output_accum = String::new();

    let debug_marker = "DEBUG OUTPUT!";
    let output_marker = "OUTPUT!!";

    let mut curr = output;

    while let Some(debug_idx) = curr.find(debug_marker) {
        // Advance past "DEBUG OUTPUT!"
        let after_debug = &curr[debug_idx + debug_marker.len()..];

        if let Some(out_idx) = after_debug.find(output_marker) {
            // Collect debug slice: from "DEBUG OUTPUT!" up to 'O' in "OUTPUT!!"
            debug_accum.push_str(&after_debug[..out_idx]);

            // Advance past "OUTPUT!!"
            let after_output = &after_debug[out_idx + output_marker.len()..];

            // The output section runs until the next "DEBUG OUTPUT!" or EOF
            if let Some(next_debug_idx) = after_output.find(debug_marker) {
                output_accum.push_str(&after_output[..next_debug_idx]);
                curr = &after_output[next_debug_idx..];
            } else {
                output_accum.push_str(after_output);
                break;
            }
        } else {
            // "DEBUG OUTPUT!" was found without a matching "OUTPUT!!", collect remainder
            debug_accum.push_str(after_debug);
            break;
        }
    }

    (debug_accum, output_accum)
}

fn no_output() -> String {
    "".to_string()
}

//get output of <cmd> for bash/zsh, for comparison with rush
fn get_output(cmd: &str) -> String {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output().unwrap().stdout;
    String::from_utf8(output).unwrap()
}

fn run_test(cmd: &str, expected: String) -> anyhow::Result<()> {
    //spawn the rust shell as a child process
    println!("{}testing{} {}", CYAN, NC, cmd.trim());
    let mut shell = Command::new(SHELL_EXE)
        .arg("--debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut shell_stdin = shell.stdin.take().expect("Failed to take shell program stdin");
    shell_stdin.write_all(cmd.as_bytes())?;
    drop(shell_stdin);

    let res = shell.wait_with_output()?;
    assert!(res.status.success());
    let (debug_output, output) = trim_debug_output(str::from_utf8(&res.stdout).unwrap_or(""));
    if output.trim() != expected.trim() {
        let stderr = str::from_utf8(&res.stderr).unwrap_or("");
        anyhow::bail!("{}\n{}\n{}", stderr, debug_output, output);
    }
    println!("{}PASS{}\n", GREEN, NC);
    Ok(())
}

#[test]
fn basic() -> anyhow::Result<()> {
    let tests = vec![
        //(<command>, <expected output>)
        ("echo 'hello world'", "hello world".to_string()),
        ("cat Cargo.toml", get_output("cat Cargo.toml")),
        ("ls ..", get_output("ls ..")),
    ];
    for test in tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn builtins() -> anyhow::Result<()> {
    //to test history, set history file to temp file. then run commands below, then run history and
    //match output
    let cwd = std::env::current_dir().unwrap();
    let parent_dir = cwd.parent().unwrap();
    let tests = vec![
        ("pwd", format!("{}", cwd.display())),
        ("cd ../ && pwd", format!("{}", parent_dir.display())),
        ("cd ~/&&pwd", format!("{}",std::env::home_dir().unwrap().display())),
    ];
    for test in tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn pipelines() -> anyhow::Result<()> {
    let tests = vec![
        // Basic 2-stage pipeline
        ("cat Cargo.toml | grep dependencies", get_output("cat Cargo.toml | grep dependencies")),
        // The classic 3-stage pipeline (Tests for File Descriptor leaks)
        ("echo 'apple banana cherry' | tr ' ' '\\n' | grep a\n", "apple\nbanana".to_string()),
        // Counting lines (verifies EOF propagation so `wc` doesn't hang)
        //("echo -e 'line1\\nline2\\nline3' | wc -l\n", "3".to_string()), <- this doesn't work
        //because bin echo doesn't know how to parse the -e flag
        ("printf 'line1\\nline2\\nline3\\n' | wc -l\n", "3".to_string()),
        // Builtins piping to external commands
        ("echo 'reverse me' | rev\n", "em esrever".to_string()),
        // Exit status propagation (false fails, but echo succeeds)
        ("false | echo 'survived'\n", "survived".to_string()),
        //  Large output buffering (prevents OS pipe buffer deadlocks)
        ("seq 1 1000 | head -n 3\n", "1\n2\n3".to_string()),
        // test builtins
    ];
    for test in tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn heredocs() -> anyhow::Result<()> {
    let heredoc_tests = vec![
        // Basic Heredoc
        (
            "cat << EOF\nhello\nworld\nEOF\n", 
            "hello\nworld".to_string()
        ),
        // Empty Heredoc: Verifies the shell handles an immediate delimiter cleanly without crashing.
        (
            "cat << EOF\nEOF\n", 
            no_output()
        ),
        // Preservation of Whitespace/Indentation: Heredocs must preserve leading spaces inside the body.
        (
            "cat << EOF\n  nested line\n    deeply nested line\nEOF\n", 
            "  nested line\n    deeply nested line".to_string()
        ),
        // Heredoc Piped into a Filter: Verifies that the heredoc contents are fed into the pipeline chain correctly.
        (
            "cat << EOF | grep target\nignore this line\nthis is the target\nskip this too\nEOF\n", 
            "this is the target".to_string()
        ),
        // Heredoc Piped into a Counter: Verifies EOF closure so tools like wc don't hang indefinitely.
        (
            "cat << EOF | wc -w\nrust language shell execution\nEOF\n", 
            "4".to_string()
        ),
        // Nested Quotes Inside Heredoc Body: The body of a standard heredoc treats quotes as literal characters, not syntax.
        (
            "cat << EOF\necho \"hello\"\nprint('world')\nEOF\n", 
            "echo \"hello\"\nprint('world')".to_string()
        ),
        //multi heredoc
        (
            "cat << A << B << C\nFirst\nA\nSecond\nB\nThird\nC\n",
            "First\nSecond\nThird".to_string()
        ),
        //no spaces between operator and delimiter
        (
            "cat <<eof\nThis should work!\neof\n", 
            "This should work!".to_string()
        ),
        //heredoc delimiter is quote
        (
            "cat <<'asdf'\nThis\nshould work!\nasdf\n",
            "This\nshould work!".to_string(),
        ),
        //dquote
        (
            "cat <<\"as\\\"df\"\nHello world\nas\"df\n",
            "Hello world".to_string(),
        ),
    ];
    for test in heredoc_tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn redirects() -> anyhow::Result<()> {
    let mut filecontent = "binturong bin\nbinder\nbingchilling";
    let mut tests: Vec<(&str, String)> = vec![
        ("echo \"binturong bin\nbinder\nbingchilling\" > temp.txt", no_output()),
        ("<temp.txt cat\n", filecontent.to_string()),
        ("grep 'binturong' < temp.txt\n", "binturong bin".to_string()),
        ("wc -l < temp.txt\n", "3".to_string()),
        ("cat   <     temp.txt    \n", filecontent.to_string()),
        //make sure builtins work with redirection as well 
        ("pwd > temp.txt\n", no_output()),
        ("history > temp.txt\n", no_output()),
        ("history >>temp.txt >temp2.txt\n", no_output()),
        ("rm temp.txt temp2.txt\n", no_output()),
    ];
    for test in tests.into_iter() {
        if let Err(e) = run_test(test.0, test.1) {
            // cleanup
            remove_file("temp.txt")?;
            remove_file("temp2.txt")?;
            anyhow::bail!(e);
        }
    }
    filecontent = "binturong bin\nbinder\nbingchilling\n";
    fs::write("temp.txt", filecontent)?;
    filecontent = "binturong bin\nbinder\nbingchilling\nappended content"; 
    tests = vec![
        //redirect operator can be anywhere in command
        (">> temp.txt echo 'appended content'\n", no_output()),
        ("<temp.txt cat\n", filecontent.to_string()),
        //multidirection redirect
        ("< temp.txt >> temp2.txt cat \n", no_output()),
        ("cat < temp2.txt\n", filecontent.to_string()),
        (">temp2.txt cat << EOF\nwowzers\nEOF\n", no_output()),
        ("cat <temp2.txt\n", "wowzers".to_string()),
        //multiple redirect in
        ("echo 'binturong' > temp.txt\n", no_output()),
        ("cat < temp.txt < temp.txt\n", "binturong\nbinturong".to_string()),
        ("cat <temp.txt <<EOF\nbingchilling\nEOF\n", "binturong\nbingchilling".to_string()),
        //multiple redirect out
        ("echo 'duplicated' > temp.txt >temp2.txt\n", no_output()),
        ("cat <temp.txt < temp2.txt\n", "duplicated\nduplicated".to_string()),
        // all 4
        ("<<EOF < temp.txt cat >> temp2.txt > temp3.txt\nbinturong\nEOF\n", no_output()),
        ("cat < temp2.txt\n", "duplicated\nbinturong\nduplicated".to_string()),
        ("cat < temp3.txt\n", "binturong\nduplicated".to_string()),
    ];
    for test in tests.into_iter() {
        if let Err(e) = run_test(test.0, test.1) {
            // cleanup
            let _ = remove_file("temp.txt");
            let _ = remove_file("temp2.txt");
            let _ = remove_file("temp3.txt");
            anyhow::bail!(e);
        }
    }
    // cleanup
    let _ = remove_file("temp.txt");
    let _ = remove_file("temp2.txt");
    let _ = remove_file("temp3.txt");
    Ok(())
}

#[test]
fn logicals() -> anyhow::Result<()> {
    let tests = vec![
        // Short-Circuit Success: Confirms && continues executing when the first command succeeds.
        (
            "true && echo \"second ran\"\n",
            "second ran\n".to_string()
        ),
        // Short-Circuit Failure: Confirms && immediately stops and skips the next command if the first fails.
        (
            "false && echo \"should not run\"\n",
            no_output()
        ),
        // Fallback Success: Confirms || stops executing if the first command succeeds (no fallback needed).
        (
            "true||echo \"should not run\"\n",
            no_output()
        ),
        // Fallback Execution: Confirms || executes the alternative command when the first fails.
        (
            "false||echo \"fallback ran\"\n",
            "fallback ran\n".to_string()
        ),
        // Left-Associative Chaining (Success Chain): Verifies complex chaining where true && true bubbles up to trigger the fallback statement only if the chain breaks.
        (
            "true && true && echo \"chain complete\" || echo \"failed\"\n",
            "chain complete\n".to_string()
        ),
        // Left-Associative Chaining (Interrupted Chain): Verifies that when a command in an && chain fails, execution breaks and cascades down to the next || block.
        (
            "true && false && echo \"skipped\" ||echo \"recovered\"\n",
            "recovered\n".to_string()
        ),
        // Deep Nesting with Output Capture: Verifies that status codes bubble up perfectly through logical junctions to allow subsequent commands to execute cleanly.
        (
            "false ||true && echo \"step 3\"&& false || echo \"final escape\"\n",
            "step 3\nfinal escape\n".to_string()
        )
    ];
    for test in tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn subshells() -> anyhow::Result<()> {
    let tests = vec![
        // Basic Subshell Isolation: Verifies a single subshell isolates commands and bubbles stdout up to the parent.
        (
            "(echo hello)\n",
            "hello\n".to_string()
        ),

        // Sequential Commands Inside Subshell: Confirms that chained operations inside the parenthesis execute sequentially.
        (
            "(echo alpha && echo beta)\n",
            "alpha\nbeta\n".to_string()
        ),

        // Basic Nesting: Verifies that a subshell can cleanly spawn and evaluate a child subshell.
        (
            "(echo outer && (echo inner))\n",
            "outer\ninner\n".to_string()
        ),

        // Deeply Nested Layers: Stress-tests the recursive post-order DFS traversal by packing multiple layers of subshell execution.
        (
            "((((echo deep))))\n",
            "deep\n".to_string()
        ),
    ];
    for test in tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn escapes() -> anyhow::Result<()> {
    let tests = vec![
        // Single Quotes: Literal preservation without processing backslashes or special characters
        (
            "echo 'hello\\nworld'\n",
            "hello\\nworld\n".to_string(),
        ),
        (
            "echo 'foo $BAR \"baz\"'\n",
            "foo $BAR \"baz\"\n".to_string(),
        ),

        // Double Quotes: Processes standard escaped control characters (\n, \t, \r, \\, \", \`, \$)
        (
            "echo \"hello\\nworld\"\n",
            "hello\nworld\n".to_string(),
        ),
        (
            "echo \"tab\\ttest\\r\"\n",
            "tab\ttest\r\n".to_string(),
        ),
        (
            "echo \"escaped \\\"quotes\\\" and \\\\ backslash\"\n",
            "escaped \"quotes\" and \\ backslash\n".to_string(),
        ),

        // POSIX Shell Fallback: Unrecognized escape sequences preserve both the backslash and character
        (
            "echo \"hello \\a world\"\n",
            "hello \\a world\n".to_string(),
        ),

        // Subshell Quote Integration: Verifies escaped double quotes inside subshells deserialize and execute cleanly
        (
            "(echo \"inner \\\"quoted\\\" value\")\n",
            "inner \"quoted\" value\n".to_string(),
        ),
    ];

    for test in tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn assignments() -> anyhow::Result<()> {
    let tests = vec![
        // Silent Assignment: Confirms standalone variable assignments produce no output.
        (
            "foo=bar\n",
            no_output()
        ),
        // Simple Global Assignment: Verifies assigning a variable and referencing it across commands.
        (
            "foo=bar\necho $foo\n",
            "bar\n".to_string()
        ),
        // Semicolon Sequential Assignments: Confirms multiple assignments separated by semicolons persist in shell state.
        (
            "foo=bar; baz=1; echo $foo$baz\n",
            "bar1\n".to_string()
        ),
        // Flexible Whitespace Around '=': Validates that spaces surrounding the assignment operator are properly handled.
        (
            "a = 1\nb=  2\nc   =3\necho $a $b $c\n",
            "1 2 3\n".to_string()
        ),
        // Double-Quote Interpolation: Ensures variable expansion occurs inside double quotes.
        (
            "foo=hello\necho \"$foo world\"\n",
            "hello world\n".to_string()
        ),
        // Single-Quote Literal Treatment: Ensures variables inside single quotes remain unexpanded raw literals.
        (
            "foo=hello\necho '$foo'\n",
            "$foo\n".to_string()
        ),
        // Complex Compound Word Part Assignment: Verifies concatenating literals, single/double quotes, and variable expansions within an assignment.
        (
            "foo=hello\nfoo=hello$foo\"world\"\"$foo\"'asdf'\necho $foo\n",
            "hellohelloworldhelloasdf\n".to_string()
        ),
        // Inline Assignment Scope: Confirms inline assignments pass into child processes without polluting parent shell state.
        (
            "FOO=bar sh -c 'echo $FOO'\necho $FOO\n",
            "bar\n\n".to_string()
        ),
    ];

    for test in tests.into_iter() {
        run_test(test.0, test.1)?;
    }
    Ok(())
}

#[test]
fn combined_operators() -> anyhow::Result<()> {
    // List of exact files created by this test so we never touch other tests' files
    let test_files = [
        "errors.log",
        "system.log",
        "audit.log",
        "result.txt",
        "file1.txt",
        "file2.txt",
        "nested_test.txt",
        "test_out.txt",
        "test_in.txt",
        "test_app.txt",
        "my file.txt",
    ];

    // Helper closure to safely remove ONLY our test files
    let cleanup = || {
        for file in &test_files {
            let _ = std::fs::remove_file(file);
        }
    };

    // Clean up any stale state before running
    cleanup();

    // SETUP
    let mut error_log = File::create("errors.log")?;
    error_log.write_all(
        b"2026-07-19 SUCCESS: Database connected successfully.\n\
         2026-07-19 ERROR: Failed to bind to interface on port 8080.\n\
         2026-07-19 WARN: High disk latency detected.\n",
    )?;
    let mut system_log = File::create("system.log")?;
    system_log.write_all(
        b"2026-07-19 SUCCESS: Database connected successfully.\n\
         2026-07-19 ERROR: Failed to bind to interface on port 8080.\n\
         2026-07-19 WARN: High disk latency detected.\n",
    )?;
    let mut audit_log = File::create("audit.log")?;
    audit_log.write_all(b"AUDIT\n")?;
    let cwd = std::env::current_dir().unwrap();
    let parent_dir = cwd.parent().unwrap();

    let tests = vec![
        // Combining Heredocs and Pipelines
        ("cat << EOF | grep 'match'\nignore\nmatch this\nignore again\nEOF\n", "match this".to_string()),

        // Subshell Isolation and Logical Chaining
        (
            "(cd ../ && pwd) && pwd\n",
            format!("{}\n{}", parent_dir.display(), cwd.display())
        ),

        // Pipeline Failure Short-Circuiting
        (
            "echo \"test data\" | grep \"match\" && echo \"found\" || echo \"not found\"\n",
            "not found".to_string()
        ),

        // Pipelined Subshells with Redirection
        (
            "(cat | grep \"critical\") <errors.log>result.txt || echo pipeline fail\n",
            "pipeline fail".to_string()
        ),
        (
            "cat result.txt\n", 
            no_output()
        ),

        // Heredoc Sequential Injection through a Pipeline into a Logical Branch
        (
            "cat << EOF1 << EOF2 | grep \"target\" && echo \"triggered\"\nalpha\ntarget\nEOF1\nbeta\nEOF2\n",
            "target\ntriggered".to_string()
        ),

        // Multi-Output Broadcast from a Complex Subshell Cascade
        (
            "(false || echo \"recovered output\") > file1.txt > file2.txt || echo \"failed completely\"\n",
            no_output() 
        ), 
        ("cat < file1.txt < file2.txt\n", "recovered output\nrecovered output".to_string()),

        // The Kitchen Sink
        (
            "(grep \"ERROR\" && echo \"found errors\"\n) < system.log >> audit.log || echo \"audit failed\"\n",
            no_output()
        ),
        (
            "cat audit.log\n",
            "AUDIT\n2026-07-19 ERROR: Failed to bind to interface on port 8080.\nfound errors".to_string()
        ),
        ("cat audit.log |\n wc -c\n", "79".to_string()),

        // Nested Subshells with Logical Short-Circuiting
        (
            "(true && (false || echo \"nested fallback\"))\n",
            "nested fallback".to_string()
        ),

        // Pipeline Interacting with Nested Subshell
        (
            "echo \"data stream\" | (cat | grep \"data\" && (echo \"deep match\"))\n",
            "data stream\ndeep match".to_string()
        ),

        // Deep AST Stress Test: Nested Subshell as First Program with Outer Redirections
        (
            "((grep \"ERROR\") && echo \"inner execution complete\") < errors.log > nested_test.txt\n",
            no_output(),
        ),
        (
            "cat nested_test.txt\n",
            "2026-07-19 ERROR: Failed to bind to interface on port 8080.\ninner execution complete\n".to_string()
        ),

        // Subshell Input/Output Redirections with Quotes
        (
            "(echo \"line 1\" && echo 'line 2') > test_out.txt && cat test_out.txt\n",
            "line 1\nline 2\n".to_string(),
        ),
        (
            "echo \"redirected input\" > test_in.txt && (cat < test_in.txt)\n",
            "redirected input\n".to_string(),
        ),

        // Subshell Append Redirection with Escaped Quotes
        (
            "echo \"first\" > test_app.txt && (echo \"second \\\"quoted\\\"\" >> test_app.txt) && cat test_app.txt\n",
            "first\nsecond \"quoted\"\n".to_string(),
        ),

        // Escaped Pipeline (|) inside Double Quotes vs Unescaped Pipeline
        (
            "echo \"arg1 | arg2\"\n",
            "arg1 | arg2\n".to_string(),
        ),
        (
            "echo \"hello world\" | (grep \"hello\")\n",
            "hello world\n".to_string(),
        ),

        // Chained Pipelines across Subshells with Quote Escapes
        (
            "(echo \"foo\\nbar\" | grep \"foo\") && (echo 'baz' | grep 'baz')\n",
            "foo\nbaz\n".to_string(),
        ),

        // Redirection Filename Containing Escaped Quotes/Spaces
        (
            "echo \"content\" > \"my file.txt\" && (cat < \"my file.txt\")\n",
            "content\n".to_string(),
        ),
    ];

    // Execute tests
    let mut res = Ok(());
    for test in tests.into_iter() {
        if let Err(e) = run_test(test.0, test.1) {
            res = Err(anyhow::anyhow!(e));
            break;
        }
    }

    // Always clean up our explicit list of files
    cleanup();
    res
}

