//! LSIF (language server index format) generator

use std::collections::HashMap;
use std::env;
use std::time::Instant;

use ide::{
    Analysis, FileId, FileRange, RootDatabase, StaticIndex, StaticIndexedFile, TokenId,
    TokenStaticData,
};
use ide_db::LineIndexDatabase;

use ide_db::base_db::salsa::{self, ParallelDatabase};
use lsp_types::{self, lsif};
use project_model::{CargoConfig, ProjectManifest, ProjectWorkspace};
use vfs::{AbsPathBuf, Vfs};

use crate::cli::{
    flags,
    load_cargo::{load_workspace, LoadCargoConfig},
    Result,
};
use crate::line_index::{LineEndings, LineIndex, OffsetEncoding};
use crate::to_proto;

/// Need to wrap Snapshot to provide `Clone` impl for `map_with`
struct Snap<DB>(DB);
impl<DB: ParallelDatabase> Clone for Snap<salsa::Snapshot<DB>> {
    fn clone(&self) -> Snap<salsa::Snapshot<DB>> {
        Snap(self.0.snapshot())
    }
}

struct LsifManager<'a> {
    count: i32,
    token_map: HashMap<TokenId, Id>,
    range_map: HashMap<FileRange, Id>,
    file_map: HashMap<FileId, Id>,
    analysis: &'a Analysis,
    db: &'a RootDatabase,
    vfs: &'a Vfs,
}

#[derive(Clone, Copy)]
struct Id(i32);

impl From<Id> for lsp_types::NumberOrString {
    fn from(Id(x): Id) -> Self {
        lsp_types::NumberOrString::Number(x)
    }
}

impl LsifManager<'_> {
    fn new<'a>(analysis: &'a Analysis, db: &'a RootDatabase, vfs: &'a Vfs) -> LsifManager<'a> {
        LsifManager {
            count: 0,
            token_map: HashMap::default(),
            range_map: HashMap::default(),
            file_map: HashMap::default(),
            analysis,
            db,
            vfs,
        }
    }

    fn add(&mut self, data: lsif::Element) -> Id {
        let id = Id(self.count);
        self.emit(&serde_json::to_string(&lsif::Entry { id: id.into(), data }).unwrap());
        self.count += 1;
        id
    }

    fn add_vertex(&mut self, vertex: lsif::Vertex) -> Id {
        self.add(lsif::Element::Vertex(vertex))
    }

    fn add_edge(&mut self, edge: lsif::Edge) -> Id {
        self.add(lsif::Element::Edge(edge))
    }

    // FIXME: support file in addition to stdout here
    fn emit(&self, data: &str) {
        println!("{}", data);
    }

    fn get_token_id(&mut self, id: TokenId) -> Id {
        if let Some(x) = self.token_map.get(&id) {
            return *x;
        }
        let result_set_id = self.add_vertex(lsif::Vertex::ResultSet(lsif::ResultSet { key: None }));
        self.token_map.insert(id, result_set_id);
        result_set_id
    }

    fn get_range_id(&mut self, id: FileRange) -> Id {
        if let Some(x) = self.range_map.get(&id) {
            return *x;
        }
        let file_id = id.file_id;
        let doc_id = self.get_file_id(file_id);
        let line_index = self.db.line_index(file_id);
        let line_index = LineIndex {
            index: line_index,
            encoding: OffsetEncoding::Utf16,
            endings: LineEndings::Unix,
        };
        let range_id = self.add_vertex(lsif::Vertex::Range {
            range: to_proto::range(&line_index, id.range),
            tag: None,
        });
        self.add_edge(lsif::Edge::Contains(lsif::EdgeDataMultiIn {
            in_vs: vec![range_id.into()],
            out_v: doc_id.into(),
        }));
        range_id
    }

    fn get_file_id(&mut self, id: FileId) -> Id {
        if let Some(x) = self.file_map.get(&id) {
            return *x;
        }
        let path = self.vfs.file_path(id);
        let path = path.as_path().unwrap();
        let doc_id = self.add_vertex(lsif::Vertex::Document(lsif::Document {
            language_id: "rust".to_string(),
            uri: lsp_types::Url::from_file_path(path).unwrap(),
        }));
        self.file_map.insert(id, doc_id);
        doc_id
    }

    fn add_token(&mut self, id: TokenId, token: TokenStaticData) {
        let result_set_id = self.get_token_id(id);
        if let Some(hover) = token.hover {
            let hover_id = self.add_vertex(lsif::Vertex::HoverResult {
                result: lsp_types::Hover {
                    contents: lsp_types::HoverContents::Markup(to_proto::markup_content(
                        hover.markup,
                    )),
                    range: None,
                },
            });
            self.add_edge(lsif::Edge::Hover(lsif::EdgeData {
                in_v: hover_id.into(),
                out_v: result_set_id.into(),
            }));
        }
        if let Some(def) = token.definition {
            let result_id = self.add_vertex(lsif::Vertex::DefinitionResult);
            let def_vertex = self.get_range_id(def);
            self.add_edge(lsif::Edge::Item(lsif::Item {
                document: (*self.file_map.get(&def.file_id).unwrap()).into(),
                property: None,
                edge_data: lsif::EdgeDataMultiIn {
                    in_vs: vec![def_vertex.into()],
                    out_v: result_id.into(),
                },
            }));
            self.add_edge(lsif::Edge::Definition(lsif::EdgeData {
                in_v: result_id.into(),
                out_v: result_set_id.into(),
            }));
        }
        if !token.references.is_empty() {
            let result_id = self.add_vertex(lsif::Vertex::ReferenceResult);
            self.add_edge(lsif::Edge::References(lsif::EdgeData {
                in_v: result_id.into(),
                out_v: result_set_id.into(),
            }));
            for x in token.references {
                let vertex = *self.range_map.get(&x.range).unwrap();
                self.add_edge(lsif::Edge::Item(lsif::Item {
                    document: (*self.file_map.get(&x.range.file_id).unwrap()).into(),
                    property: Some(if x.is_definition {
                        lsif::ItemKind::Definitions
                    } else {
                        lsif::ItemKind::References
                    }),
                    edge_data: lsif::EdgeDataMultiIn {
                        in_vs: vec![vertex.into()],
                        out_v: result_id.into(),
                    },
                }));
            }
        }
    }

    fn add_file(&mut self, file: StaticIndexedFile) {
        let StaticIndexedFile { file_id, tokens, folds } = file;
        let doc_id = self.get_file_id(file_id);
        let text = self.analysis.file_text(file_id).unwrap();
        let line_index = self.db.line_index(file_id);
        let line_index = LineIndex {
            index: line_index,
            encoding: OffsetEncoding::Utf16,
            endings: LineEndings::Unix,
        };
        let result = folds
            .into_iter()
            .map(|it| to_proto::folding_range(&*text, &line_index, false, it))
            .collect();
        let folding_id = self.add_vertex(lsif::Vertex::FoldingRangeResult { result });
        self.add_edge(lsif::Edge::FoldingRange(lsif::EdgeData {
            in_v: folding_id.into(),
            out_v: doc_id.into(),
        }));
        let tokens_id = tokens
            .into_iter()
            .map(|(range, id)| {
                let range_id = self.add_vertex(lsif::Vertex::Range {
                    range: to_proto::range(&line_index, range),
                    tag: None,
                });
                self.range_map.insert(FileRange { file_id, range }, range_id);
                let result_set_id = self.get_token_id(id);
                self.add_edge(lsif::Edge::Next(lsif::EdgeData {
                    in_v: result_set_id.into(),
                    out_v: range_id.into(),
                }));
                range_id.into()
            })
            .collect();
        self.add_edge(lsif::Edge::Contains(lsif::EdgeDataMultiIn {
            in_vs: tokens_id,
            out_v: doc_id.into(),
        }));
    }
}

impl flags::Lsif {
    pub fn run(self) -> Result<()> {
        eprintln!("Generating LSIF started...");
        let now = Instant::now();
        let cargo_config = CargoConfig::default();
        let no_progress = &|_| ();
        let load_cargo_config = LoadCargoConfig {
            load_out_dirs_from_check: true,
            with_proc_macro: true,
            prefill_caches: false,
        };
        let path = AbsPathBuf::assert(env::current_dir()?.join(&self.path));
        let manifest = ProjectManifest::discover_single(&path)?;

        let workspace = ProjectWorkspace::load(manifest, &cargo_config, no_progress)?;

        let (host, vfs, _proc_macro) = load_workspace(workspace, &load_cargo_config)?;
        let db = host.raw_database();
        let analysis = host.analysis();

        let si = StaticIndex::compute(db, &analysis);

        let mut lsif = LsifManager::new(&analysis, db, &vfs);
        lsif.add_vertex(lsif::Vertex::MetaData(lsif::MetaData {
            version: String::from("0.5.0"),
            project_root: lsp_types::Url::from_file_path(path).unwrap(),
            position_encoding: lsif::Encoding::Utf16,
            tool_info: Some(lsp_types::lsif::ToolInfo {
                name: "rust-analyzer".to_string(),
                args: vec![],
                version: Some(env!("REV").to_string()),
            }),
        }));
        for file in si.files {
            lsif.add_file(file);
        }
        for (id, token) in si.tokens.iter() {
            lsif.add_token(id, token);
        }
        eprintln!("Generating LSIF finished in {:?}", now.elapsed());
        Ok(())
    }
}
