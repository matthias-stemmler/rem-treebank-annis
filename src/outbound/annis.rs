use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;

use anyhow::{anyhow, bail, ensure};
use graphannis::corpusstorage::{ExportFormat, QueryLanguage, ResultOrder, SearchQuery};
pub(crate) use graphannis::model::AnnotationComponentType;
use graphannis::util::node_names_from_match;
use graphannis_core::graph::update::{GraphUpdate, UpdateEvent};
use graphannis_core::graph::NODE_NAME;
pub(crate) use graphannis_core::graph::{ANNIS_NS, DEFAULT_NS};
use itertools::Itertools;
use regex::Regex;
use tempfile::NamedTempFile;
use tracing::info;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

use crate::{annis_util, inbound};

pub(crate) const LAYER: &str = "layer";
pub(crate) const NODE: &str = "node";

pub(crate) struct CorpusWriter<'a> {
    corpus_count: usize,
    path: &'a Path,
    zip_writer: ZipWriter<NamedTempFile>,
}

impl<'a> CorpusWriter<'a> {
    pub(crate) fn new(path: &'a Path) -> anyhow::Result<Self> {
        Ok(Self {
            corpus_count: 0,
            path,
            zip_writer: ZipWriter::new(NamedTempFile::new_in(
                path.parent()
                    .ok_or_else(|| anyhow!("path {} has no parent", path.display()))?,
            )?),
        })
    }

    pub(crate) fn write_corpus(
        &mut self,
        corpus: &Corpus<'_>,
        config: &toml::Table,
    ) -> anyhow::Result<()> {
        info!(corpus_name = &*corpus.name, "writing corpus");

        let temp_dir = tempfile::tempdir()?;

        corpus.storage.export_to_fs(
            &[&corpus.original_name],
            temp_dir.path(),
            ExportFormat::GraphMLDirectory,
        )?;

        let graphml_string = {
            let mut graphml_string = fs::read_to_string(
                temp_dir
                    .path()
                    .join(format!("{}.graphml", corpus.original_name)),
            )?;

            let range = CDATA_REGEX
                .find_iter(&graphml_string)
                .exactly_one()
                .map_err(|err| anyhow::Error::msg(err.to_string()))?
                .range();

            graphml_string.replace_range(
                range,
                &format!("<![CDATA[{}]]>", toml::to_string_pretty(&config)?),
            );

            graphml_string
        };

        self.zip_writer.start_file(
            format!("{}.graphml", corpus.name),
            SimpleFileOptions::default(),
        )?;

        self.zip_writer.write_all(graphml_string.as_bytes())?;

        let linked_files_dir = temp_dir.path().join(&*corpus.name);

        if linked_files_dir.exists() {
            for entry in fs::read_dir(linked_files_dir)? {
                let entry = entry?;

                if entry.file_type()?.is_file() {
                    self.zip_writer.start_file_from_path(
                        Path::new(&*corpus.name).join(entry.file_name()),
                        SimpleFileOptions::default(),
                    )?;
                    io::copy(&mut File::open(entry.path())?, &mut self.zip_writer)?;
                } else {
                    bail!(
                        "unexpected file {} in corpus export",
                        entry.path().display(),
                    );
                }
            }
        }

        // unload corpus to free memory
        corpus.storage.unload(corpus.original_name)?;

        self.corpus_count += 1;

        Ok(())
    }

    pub(crate) fn finish(self) -> anyhow::Result<()> {
        self.zip_writer.finish()?.persist(self.path)?;

        info!(
            path = %self.path.display(),
            count = self.corpus_count,
            "written corpora",
        );

        Ok(())
    }
}

pub(crate) struct Corpus<'a> {
    storage: Rc<annis_util::TempStorage>,
    original_name: &'a str,
    name: Cow<'a, str>,
}

impl<'a> Corpus<'a> {
    pub(crate) fn from_inbound_corpus(corpus: &'a inbound::annis::Corpus<'_>) -> Self {
        Self {
            storage: Rc::clone(corpus.storage()),
            original_name: corpus.name(),
            name: corpus.name().into(),
        }
    }

    pub(crate) fn begin_update(&self) -> Update<'_> {
        Update {
            corpus: self,
            update: Some(GraphUpdate::new()),
        }
    }

    pub(crate) fn update_name(&mut self, op: impl FnOnce(&str) -> String) -> anyhow::Result<()> {
        let new_name = op(&self.name);

        let name_encoded = urlencoding::encode(&self.name);
        let new_name_encoded = urlencoding::encode(&new_name);

        info!(old_name = &*self.name, new_name, "renaming corpus");

        let mut update = self.begin_update();

        for m in self.query("annis:node_name")? {
            let node_name = m
                .into_iter()
                .exactly_one()
                .map_err(|_| anyhow!("unexpected number of nodes in query match"))?;

            let new_node_name = if node_name == self.name {
                // node name of corpus node is *not* URL-encoded
                new_name.clone()
            } else if let Some((corpus_name_encoded, rest)) = node_name.split_once('/') {
                // corpus name within node name of non-corpus node *is* URL-encoded
                ensure!(
                    corpus_name_encoded == name_encoded,
                    "unexpected corpus name in node name: '{}' != '{}'",
                    corpus_name_encoded,
                    name_encoded,
                );
                format!("{new_name_encoded}/{rest}")
            } else {
                bail!("unexpected node name: '{node_name}'");
            };

            update.add_node_anno(node_name, ANNIS_NS.into(), NODE_NAME.into(), new_node_name)?;
        }

        update.apply()?;
        self.name = new_name.into();

        Ok(())
    }

    pub(crate) fn query(&self, query: &str) -> anyhow::Result<impl Iterator<Item = Vec<String>>> {
        Ok(self
            .storage
            .find(
                SearchQuery {
                    corpus_names: &[&self.original_name],
                    query,
                    query_language: QueryLanguage::AQL,
                    timeout: None,
                },
                0,
                None,
                ResultOrder::Normal,
            )?
            .into_iter()
            .map(|m| node_names_from_match(&m)))
    }
}

pub(crate) struct Update<'a> {
    corpus: &'a Corpus<'a>,
    update: Option<GraphUpdate>,
}

impl Update<'_> {
    pub(crate) fn add_node(&mut self, node_name: String, node_type: String) -> anyhow::Result<()> {
        Ok(self
            .update
            .as_mut()
            .unwrap()
            .add_event(UpdateEvent::AddNode {
                node_name,
                node_type,
            })?)
    }

    pub(crate) fn add_node_anno(
        &mut self,
        node_name: String,
        anno_ns: String,
        anno_name: String,
        anno_value: String,
    ) -> anyhow::Result<()> {
        Ok(self
            .update
            .as_mut()
            .unwrap()
            .add_event(UpdateEvent::AddNodeLabel {
                node_name,
                anno_ns,
                anno_name,
                anno_value,
            })?)
    }

    pub(crate) fn add_edge(
        &mut self,
        source_node: String,
        target_node: String,
        component_type: &AnnotationComponentType,
        layer: String,
        component_name: String,
    ) -> anyhow::Result<()> {
        Ok(self
            .update
            .as_mut()
            .unwrap()
            .add_event(UpdateEvent::AddEdge {
                source_node,
                target_node,
                layer,
                component_type: component_type.to_string(),
                component_name,
            })?)
    }

    pub(crate) fn apply(mut self) -> anyhow::Result<()> {
        let mut update = self.update.take().unwrap();

        info!(
            corpus_name = &*self.corpus.name,
            count = update.len()?,
            "applying updates to corpus",
        );

        Ok(self
            .corpus
            .storage
            .apply_update(self.corpus.original_name, &mut update)?)
    }
}

static CDATA_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<!\[CDATA\[(?s:.)*?]]>").unwrap());
