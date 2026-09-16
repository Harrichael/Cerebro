use std::path::Path;
use std::process::Command;

use crate::dialect::{Dialect, starts_with_word};
use crate::indexer::Indexer;

pub struct ScipGo;

impl Dialect for ScipGo {
    fn name(&self) -> &'static str {
        "scip-go"
    }

    /// Go writes imports either as a parenthesised block or one per line, and
    /// both forms have to be recognised: a single-line `import "x"` that went
    /// unmarked would read as an ordinary use of the package.
    fn import_lines(&self, text: &str) -> Vec<bool> {
        let mut in_block = false;
        text.lines()
            .map(|raw| {
                let line = raw.trim_start();
                if in_block {
                    if line.starts_with(')') {
                        in_block = false;
                    }
                    return true;
                }
                if line.starts_with("import (") {
                    in_block = true;
                    return true;
                }
                starts_with_word(line, "import")
            })
            .collect()
    }
}

impl Indexer for ScipGo {
    fn manifests(&self) -> &'static [&'static str] {
        &["go.mod"]
    }

    fn tool(&self) -> &'static str {
        "scip-go"
    }

    fn install_hint(&self) -> &'static str {
        "go install github.com/scip-code/scip-go/cmd/scip-go@latest"
    }

    fn command(&self, out: &Path) -> Command {
        let mut cmd = Command::new(self.tool());
        cmd.arg("--output").arg(out);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_lines_cover_the_block_and_the_single_line_form() {
        let text = "package main\n\nimport \"math\"\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/x\"\n)\n\nfunc f() { fmt.Println() }\n";
        assert_eq!(
            ScipGo.import_lines(text),
            [false, false, true, false, true, true, true, true, true, false, false]
        );
    }
}
