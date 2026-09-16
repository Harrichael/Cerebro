use std::path::Path;
use std::process::Command;

use crate::dialect::{Dialect, starts_with_word};
use crate::indexer::Indexer;
use crate::source::ColumnUnit;

pub struct ScipTypescript;

impl Dialect for ScipTypescript {
    fn name(&self) -> &'static str {
        "scip-typescript"
    }

    /// Only the opening line of a multi-line `import { .. } from "x"` is
    /// marked, so names on the inner lines read as TypeRef or Call.
    fn import_lines(&self, text: &str) -> Vec<bool> {
        text.lines()
            .map(|raw| starts_with_word(strip_export(raw.trim_start()), "import"))
            .collect()
    }

    /// scip-typescript predates `position_encoding` and counts UTF-16 code
    /// units, JavaScript's own string unit.
    fn unspecified_column_unit(&self) -> ColumnUnit {
        ColumnUnit::Utf16
    }
}

impl Indexer for ScipTypescript {
    fn manifests(&self) -> &'static [&'static str] {
        &["tsconfig.json", "package.json"]
    }

    fn tool(&self) -> &'static str {
        "npx"
    }

    fn install_hint(&self) -> &'static str {
        "install Node.js; npx fetches @sourcegraph/scip-typescript itself"
    }

    fn command(&self, out: &Path) -> Command {
        let mut cmd = Command::new(self.tool());
        cmd.args(["--yes", "@sourcegraph/scip-typescript", "index", "--output"]).arg(out);
        cmd
    }
}

fn strip_export(line: &str) -> &str {
    match line.strip_prefix("export") {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_lines_are_import_statements_with_or_without_export() {
        let text = "import { Point } from \"./geometry\";\nexport import x = ns.y;\nimportant();\n} from \"./geometry\";\n";
        assert_eq!(
            ScipTypescript.import_lines(text),
            [true, true, false, false]
        );
    }
}
