/// Events emitted by language coordination operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageEvent {
    Log {
        level: LanguageLogLevel,
        message: String,
    },
    ShowMessage {
        level: LanguageLogLevel,
        message: String,
    },
    SemanticTokensRefresh {
        language_id: String,
    },
}

impl LanguageEvent {
    pub fn log(level: LanguageLogLevel, message: impl Into<String>) -> Self {
        Self::Log {
            level,
            message: message.into(),
        }
    }

    pub fn show_message(level: LanguageLogLevel, message: impl Into<String>) -> Self {
        Self::ShowMessage {
            level,
            message: message.into(),
        }
    }

    pub fn semantic_tokens_refresh(language_id: impl Into<String>) -> Self {
        Self::SemanticTokensRefresh {
            language_id: language_id.into(),
        }
    }
}

/// Log levels abstracted from LSP message types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageLogLevel {
    Error,
    Warning,
    Info,
}

/// Result of an individual language load attempt
#[derive(Debug, Default)]
pub struct LanguageLoadResult {
    pub success: bool,
    pub events: Vec<LanguageEvent>,
}

impl LanguageLoadResult {
    pub fn failure_with(event: LanguageEvent) -> Self {
        Self {
            success: false,
            events: vec![event],
        }
    }

    pub fn success_with(events: Vec<LanguageEvent>) -> Self {
        Self {
            success: true,
            events,
        }
    }
}

/// Summary of applying configuration across multiple languages
#[derive(Debug, Default)]
pub struct LanguageLoadSummary {
    pub loaded: Vec<String>,
    pub events: Vec<LanguageEvent>,
}

impl LanguageLoadSummary {
    pub fn record(&mut self, language: &str, result: LanguageLoadResult) {
        if result.success {
            self.loaded.push(language.to_string());
        }
        self.events.extend(result.events);
    }
}
