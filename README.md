# RUSH - RUst SHell Interpreter

A bash-like cli shell and shell-script interpreter. It utilizes a stateful lexer, recursive descent parsing, and AST-based execution to handle shell grammar, piping, and IO redirection. This project was not vibe coded <small>(except for this .md file because screw markdown syntax lol)</small>.

## Usage

* **`cargo r -- --debug`**: Starts the shell in debug mode, which shows the tokenized output of your input commands.
* **`cargo r`** (or `cargo run`): Starts the regular CLI shell.
* **`cargo t -- --nocapture --test-threads=1`**: Runs all tests sequentially. For each test, this prints out exactly what command is being run and outputs the tokenized input to the terminal.
* **`cargo t`** (or `cargo test`): Runs all unit and integration tests in parallel.

## Features

### String Allocation Minimization
* Minimizes heap allocations by heavily utilizing Rust lifetimes (`&'a str` and `Cow<'a, str>`).
* The core pipeline (Shell <-> Lexer -> Parser -> Executor) relies almost entirely on string slices that reference the original user input command buffer.
* `String` type allocations for command arguments are delayed until the execution phase after all grammar has been verified, except for when quoted content must be interpreted immediately


### Prompt Continuation
* The grammar-aware lexer can prompt the shell REPL loop to continue input if it detects an incomplete statement.
* This triggers if the input is missing a closing bracket, missing a closing quote, missing a closing delimiter for a heredoc, or ends with a pipe operator.
* Example Process:
    1. User inputs an unclosed heredoc: `$ cat << EOF`
    2. The lexer yields a continuation state because `EOF` has not been found.
    3. The shell prompts for more input:
    `> line 1`
    `> line 2`
    `> EOF`
    4. Once the delimiter is reached, parsing and execution proceed.


### Assignments
* **Global Shell State:** Running a standalone assignment (e.g., `FOO=bar`) sets the variable inside the active REPL shell state, persisting across subsequent commands in the session.
* **Inline Process Environment:** Prefixing an executable command with an assignment (e.g., `FOO=bar sh -c 'echo $FOO'`) temporarily sets the environment variable for that spawned child process only, leaving the parent shell environment untouched.
* **Space-Tolerant Grammar:** Unlike traditional POSIX shells (like Bash or Zsh) where spaces around the equal sign cause syntax errors or attempt to execute the variable name as a command (`x = y` failing), this parser natively handles whitespace around assignments. Both `KEY=value` and `KEY = value` work seamlessly.


### Supported Shell Operators
* **Redirects (`<`, `>`, `>>`):** Route standard input and output to or from files.
    * *Example:* `grep "error" < /var/log/syslog >> errors.txt > backup.txt` should grab the stdout of grep, append to errors.txt, and write it to a new backup.txt file
    * Multiple stdin redirection and stdout redirection is supported. In the following example, first the contents of the stream from the heredoc, then `temp.txt`, will be appended to `temp2.txt` and truncate `temp3.txt`.
        ```bash
        <<EOF < temp.txt cat >> temp2.txt > temp3.txt
        binturong
        EOF
        ```


* **Logical Operators (`&&`, `||`):** Chain commands conditionally based on the exit status of the previous command.
    * *Example:* `cargo build && cargo test || echo "Build or tests failed!"`


* **Subshells (`()`):** Execute a sequence of commands in an isolated child process by spawning a duplicate shell, leaving the parent's environment intact.
    * *Example:* `(cd /tmp && wget http://example.com/file.zip && unzip file.zip) && ls`


* **Pipes (`|`):** Pass the standard output of one command directly into the standard input of the next.
    * *Example:* `cat README.md | grep "Rust" | wc -l`


* **Heredocs (`<<`):** Stream inline, multi-line string payloads into a command's standard input.
    *   *Sequential Example:*
        ```bash
        cat << ONE << TWO << THREE
        First
        ONE
        Second
        TWO
        Third
        THREE
        # Output:
        # First
        # Second
        # Third
        ```
    *   *Pipelined Example:*
        ```bash
        cat << A | cat << B | cat << C
        Apples
        A
        Bananas
        B
        Cherries
        C
        # Output:
        # Cherries
        ```

## Project Structure

```text
.
├── Cargo.toml
├── Cargo.lock
├── package.json
├── package-lock.json
├── vite.config.ts
├── index.html         # Tauri frontend
├── css/               # Styling
├── js/                # Typescript 
├── node_modules/
├── src/               # Core shell program, decoupled from tauri
│   ├── lexer.rs       # Stateful lexer utilizing the logos crate
│   ├── parser.rs      # Recursive descent parser that builds the AST
│   ├── executor.rs    # execution engine for process and I/O management
│   └── shell.rs       # REPL loop and environment state management
├── src-tauri/         # Tauri project (for a GUI; still work in progress)
    ├── src/
        ├── lib.rs
        ├── main.rs
    ├── Cargo.toml
    ├── etc.
├── target/            
├── tests/
│   └── integration_tests.rs  # test suite
├── test_out.txt       # test suite output: 'cargo t -- --no-capture --test-threads=1'
└── README.md
