#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOptions {
    pub color: ColorChoice,
    pub verbose: bool,
    pub target: Option<String>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            color: ColorChoice::Auto,
            verbose: false,
            target: None,
        }
    }
}
