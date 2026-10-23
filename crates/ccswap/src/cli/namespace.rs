//! `ccswap <namespace> <verb> ...`: one provider's commands under its own name.

pub struct Namespace {
    pub name: &'static str,
    /// What follows `<prog> <name>` on the usage line.
    pub usage: &'static str,
    pub help: &'static str,
}

impl Namespace {
    pub fn usage_error(&self, prog: &str, msg: &str) -> i32 {
        eprintln!("usage: {prog} {} {}", self.name, self.usage);
        eprintln!("{prog} {}: error: {msg}", self.name);
        2
    }

    /// The verb `find` knows and the arguments after it. `Err` holds the exit
    /// code once the namespace help or a usage error has been printed.
    pub fn verb<'a, T>(
        &self,
        prog: &str,
        argv: &'a [String],
        find: impl Fn(&str) -> Option<T>,
    ) -> Result<(T, &'a [String]), i32> {
        let name = self.name;
        let Some((verb, rest)) = argv.split_first() else {
            return Err(self.usage_error(
                prog,
                &format!("no command given — try '{prog} {name} --help'"),
            ));
        };
        if matches!(verb.as_str(), "-h" | "--help" | "help") {
            print!("{}", self.help.replace("{prog}", prog));
            return Err(0);
        }
        match find(verb) {
            Some(found) => Ok((found, rest)),
            None => Err(self.usage_error(
                prog,
                &format!("unknown command '{verb}' — try '{prog} {name} --help'"),
            )),
        }
    }
}
