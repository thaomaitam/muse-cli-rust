impl Command {
    pub fn leaf_name(&self) -> &'static str {
        match self {
            Command::Alpha => "alpha",
            Command::Beta => "beta",
            Command::GammaDelta => "gamma-delta",
            Command::Fs { op } => match op {
                Some(FsCommand::Ls { .. }) => "fs ls",
                None => "fs",
            },
            Command::Raw => "raw",
        }
    }
}
