use dashmap::DashMap;
use glob::glob;
use ropey::Rope;
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};
use veryl_analyzer::symbol_table::Name;
use veryl_analyzer::{namespace_table, symbol_table, Analyzer};
use veryl_formatter::Formatter;
use veryl_metadata::Metadata;
use veryl_parser::veryl_token::Token;
use veryl_parser::veryl_walker::VerylWalker;
use veryl_parser::{miette, resource_table, Finder, Parser, ParserError};

#[derive(Debug)]
pub struct Backend {
    client: Client,
    root_uri: Mutex<Option<Url>>,
    document_map: DashMap<String, Rope>,
    parser_map: DashMap<String, Parser>,
}

struct TextDocumentItem {
    uri: Url,
    text: String,
    version: i32,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            root_uri: Mutex::new(None),
            document_map: DashMap::new(),
            parser_map: DashMap::new(),
        }
    }

    async fn on_change(&self, params: TextDocumentItem) {
        let path = params.uri.to_string();
        let rope = Rope::from_str(&params.text);
        let text = rope.to_string();

        let diag = match Parser::parse(&text, &path) {
            Ok(x) => {
                if let Some(path) = resource_table::get_path_id(Path::new(&path).to_path_buf()) {
                    symbol_table::drop(path);
                    namespace_table::drop(path);
                }
                let mut analyzer = Analyzer::new(&text);
                let mut errors = analyzer.analyze(&x.veryl);
                let ret: Vec<_> = errors
                    .drain(0..)
                    .map(|x| {
                        let x: miette::ErrReport = x.into();
                        Backend::to_diag(x, &rope)
                    })
                    .collect();
                self.parser_map.insert(path.clone(), x);
                ret
            }
            Err(x) => {
                self.parser_map.remove(&path);
                vec![Backend::to_diag(x, &rope)]
            }
        };
        self.client
            .publish_diagnostics(params.uri, diag, Some(params.version))
            .await;

        self.document_map.insert(path, rope);
    }

    async fn background_analyze(&self, path: &Path) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(uri) = Url::from_file_path(path) {
                let path = uri.to_string();
                if self.document_map.contains_key(&path) {
                    return;
                }
                if let Ok(x) = Parser::parse(&text, &path) {
                    if let Some(path) = resource_table::get_path_id(Path::new(&path).to_path_buf())
                    {
                        symbol_table::drop(path);
                        namespace_table::drop(path);
                    }
                    let mut analyzer = Analyzer::new(&text);
                    let _ = analyzer.analyze(&x.veryl);
                    self.client
                        .log_message(MessageType::INFO, format!("background_analyze: {}", path))
                        .await;
                }
            }
        }
    }

    fn to_diag(err: miette::ErrReport, rope: &Rope) -> Diagnostic {
        let miette_diag: &dyn miette::Diagnostic = err.as_ref();

        let range = if let Some(mut labels) = miette_diag.labels() {
            labels.next().map_or(Range::default(), |label| {
                let line = rope.byte_to_line(label.offset());
                let pos = label.offset() - rope.line_to_byte(line);
                let line = line as u32;
                let pos = pos as u32;
                let len = label.len() as u32;
                Range::new(Position::new(line, pos), Position::new(line, pos + len))
            })
        } else {
            Range::default()
        };

        let code = miette_diag
            .code()
            .map(|d| NumberOrString::String(format!("{d}")));

        let message = if let Some(x) = err.downcast_ref::<ParserError>() {
            match x {
                ParserError::PredictionErrorWithExpectations {
                    unexpected_tokens, ..
                } => {
                    format!(
                        "Syntax Error: {}",
                        Backend::demangle_unexpected_token(&unexpected_tokens[0].to_string())
                    )
                }
                _ => format!("Syntax Error: {}", x),
            }
        } else {
            format!("Semantic Error: {}", err)
        };

        Diagnostic::new(
            range,
            Some(DiagnosticSeverity::ERROR),
            code,
            Some(String::from("veryl-ls")),
            message,
            None,
            None,
        )
    }

    fn demangle_unexpected_token(text: &str) -> String {
        if text.contains("LBracketAMinusZ") {
            String::from("Unexpected token: Identifier")
        } else if text.contains("LBracket0Minus") {
            String::from("Unexpected token: Number")
        } else {
            text.replace("LA(1) (", "").replace(')', "")
        }
    }

    fn to_location(token: &Token) -> Location {
        let line = token.line as u32 - 1;
        let column = token.column as u32 - 1;
        let length = token.length as u32;
        let uri = Url::parse(&token.file_path.to_string()).unwrap();
        let range = Range::new(
            Position::new(line, column),
            Position::new(line, column + length),
        );
        Location { uri, range }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(root_uri) = params.root_uri {
            if let Ok(mut x) = self.root_uri.lock() {
                x.replace(root_uri);
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                definition_provider: Some(OneOf::Left(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: String::from("veryl-ls"),
                version: Some(String::from(env!("CARGO_PKG_VERSION"))),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;

        let root = if let Ok(x) = self.root_uri.lock() {
            if let Some(ref x) = *x {
                if x.scheme() == "file" {
                    x.to_file_path().ok()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(root) = root {
            let glob_pattern = format!("{}/**/*.vl", root.to_string_lossy());
            let register_options = format!(
                "{{ \"watchers\": [ {{\"globPattern\": \"{}\"}} ] }}",
                glob_pattern
            );
            let register_options: Value = serde_json::from_str(&register_options).unwrap();

            let registration = Registration {
                id: "workspace/didChangeWatchedFiles".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: Some(register_options),
            };
            let _ = self.client.register_capability(vec![registration]).await;

            for entry in glob(&glob_pattern).unwrap().flatten() {
                self.background_analyze(&entry).await;
            }
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.client.log_message(MessageType::INFO, "did_open").await;

        self.on_change(TextDocumentItem {
            uri: params.text_document.uri,
            text: params.text_document.text,
            version: params.text_document.version,
        })
        .await
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "did_change")
            .await;

        self.on_change(TextDocumentItem {
            uri: params.text_document.uri,
            text: std::mem::take(&mut params.content_changes[0].text),
            version: params.text_document.version,
        })
        .await
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("did_change_watched_files: {:?}", change),
                )
                .await;
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let path = uri.to_string();
        if let Some(parser) = self.parser_map.get(&path) {
            let mut finder = Finder::new();
            finder.line = params.text_document_position_params.position.line as usize + 1;
            finder.column = params.text_document_position_params.position.character as usize + 1;
            finder.veryl(&parser.veryl);
            if let Some(token) = finder.token {
                if let Some(namespace) = namespace_table::get(token.id) {
                    let name = Name::Hierarchical(vec![token.text]);
                    if let Some(symbol) = symbol_table::get(&name, &namespace) {
                        let location = Backend::to_location(&symbol.token);
                        return Ok(Some(GotoDefinitionResponse::Scalar(location)));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let mut ret = Vec::new();
        for symbol in symbol_table::get_all() {
            let name = symbol.token.text.to_string();
            if name.contains(&params.query) {
                let kind = match symbol.kind {
                    veryl_analyzer::symbol::SymbolKind::Port(_) => SymbolKind::VARIABLE,
                    veryl_analyzer::symbol::SymbolKind::Variable(_) => SymbolKind::VARIABLE,
                    veryl_analyzer::symbol::SymbolKind::Module(_) => SymbolKind::MODULE,
                    veryl_analyzer::symbol::SymbolKind::Interface(_) => SymbolKind::INTERFACE,
                    veryl_analyzer::symbol::SymbolKind::Function(_) => SymbolKind::FUNCTION,
                    veryl_analyzer::symbol::SymbolKind::Parameter(_) => SymbolKind::CONSTANT,
                    veryl_analyzer::symbol::SymbolKind::Instance(_) => SymbolKind::OBJECT,
                    veryl_analyzer::symbol::SymbolKind::Block => SymbolKind::NAMESPACE,
                    veryl_analyzer::symbol::SymbolKind::Package => SymbolKind::PACKAGE,
                };
                let location = Backend::to_location(&symbol.token);
                #[allow(deprecated)]
                let symbol_info = SymbolInformation {
                    name,
                    kind,
                    tags: None,
                    deprecated: None,
                    location,
                    container_name: None,
                };
                ret.push(symbol_info);
            }
        }
        Ok(Some(ret))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let path = uri.to_string();
        if let Some(parser) = self.parser_map.get(&path) {
            let mut finder = Finder::new();
            finder.line = params.text_document_position_params.position.line as usize + 1;
            finder.column = params.text_document_position_params.position.character as usize + 1;
            finder.veryl(&parser.veryl);
            if let Some(token) = finder.token {
                if let Some(namespace) = namespace_table::get(token.id) {
                    let name = Name::Hierarchical(vec![token.text]);
                    if let Some(symbol) = symbol_table::get(&name, &namespace) {
                        let text = symbol.kind.to_string();
                        let hover = Hover {
                            contents: HoverContents::Scalar(MarkedString::String(text)),
                            range: None,
                        };
                        return Ok(Some(hover));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let path = uri.to_string();
        if let Ok(metadata_path) = Metadata::search_from(uri.path()) {
            if let Ok(metadata) = Metadata::load(metadata_path) {
                if let Some(rope) = self.document_map.get(&path) {
                    let line = rope.len_lines() as u32;
                    if let Some(parser) = self.parser_map.get(&path) {
                        let mut formatter = Formatter::new(&metadata);
                        formatter.format(&parser.veryl);

                        let text_edit = TextEdit {
                            range: Range::new(Position::new(0, 0), Position::new(line, u32::MAX)),
                            new_text: formatter.as_str().to_string(),
                        };

                        return Ok(Some(vec![text_edit]));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
