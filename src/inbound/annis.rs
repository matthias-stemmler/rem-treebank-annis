use std::borrow::Cow;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;
use std::{fmt, vec};

use anyhow::anyhow;
use graphannis::corpusstorage::{QueryLanguage, ResultOrder, SearchQuery};
use graphannis::graph::{Component, NodeID};
use graphannis::model::{AnnotationComponent, AnnotationComponentType};
use graphannis::util::node_names_from_match;
use graphannis::AnnotationGraph;
use graphannis_core::graph::{ANNIS_NS, DEFAULT_NS, NODE_NAME_KEY};
pub(crate) use graphannis_core::types::AnnoKey;
use itertools::Itertools;
use tracing::info;

use crate::annis_util;

static DEFAULT_ORDERING_COMPONENT: LazyLock<AnnotationComponent> = LazyLock::new(|| {
    Component::new(
        AnnotationComponentType::Ordering,
        ANNIS_NS.into(),
        "".into(),
    )
});

pub(crate) struct Storage {
    storage: Rc<annis_util::TempStorage>,
    corpus_names: Vec<String>,
}

impl Storage {
    pub(crate) fn from_zip(path: &Path, in_memory: bool) -> anyhow::Result<Self> {
        info!(path = %path.display(), in_memory, "importing corpora");

        let storage = Rc::new(annis_util::TempStorage::new()?);

        let corpus_names = storage.import_all_from_zip(
            File::open(path)?,
            !in_memory,
            false, /* overwrite_existing */
            |msg| info!("{msg}"),
        )?;

        info!(count = corpus_names.len(), "imported corpora");

        Ok(Self {
            storage,
            corpus_names,
        })
    }

    pub(crate) fn corpora(&self) -> impl Iterator<Item = Corpus<'_>> {
        self.corpus_names.iter().map(|name| Corpus {
            storage: Rc::clone(&self.storage),
            name,
        })
    }
}

pub(crate) struct Corpus<'a> {
    storage: Rc<annis_util::TempStorage>,
    name: &'a str,
}

impl<'a> Corpus<'a> {
    pub(crate) fn storage(&self) -> &Rc<annis_util::TempStorage> {
        &self.storage
    }

    pub(crate) fn name(&self) -> &str {
        self.name
    }

    pub(crate) fn config(&self) -> anyhow::Result<toml::Table> {
        Ok(toml::Table::try_from(self.storage.info(self.name)?.config)?)
    }

    pub(crate) fn documents(
        &self,
    ) -> anyhow::Result<impl Iterator<Item = anyhow::Result<Document>> + '_> {
        let matches = self.storage.find(
            SearchQuery {
                corpus_names: &[self.name],
                query: "annis:doc",
                query_language: QueryLanguage::AQL,
                timeout: None,
            },
            0,
            None,
            ResultOrder::Normal,
        )?;

        Ok(matches.into_iter().map(|m| {
            let node_name = node_names_from_match(&m).into_iter().exactly_one()?;

            Ok(Document {
                graph: self
                    .storage
                    .subcorpus_graph(self.name, vec![node_name.clone()])?,
                node_name,
            })
        }))
    }
}

pub(crate) struct Document {
    graph: AnnotationGraph,
    node_name: String,
}

impl Document {
    pub(crate) fn node_name(&self) -> NodeName<'_> {
        NodeName(Cow::Borrowed(&self.node_name))
    }

    pub(crate) fn doc_name(&self) -> anyhow::Result<&str> {
        let (_, doc_name) = self.node_name.split_once('/').ok_or_else(|| {
            anyhow!(
                "could not get document name from node name {}",
                self.node_name
            )
        })?;

        Ok(doc_name)
    }

    pub(crate) fn segmentation_nodes_in_order(
        &self,
        segmentation: &str,
    ) -> anyhow::Result<Nodes<'_>> {
        let ordering_storage = self
            .graph
            .get_graphstorage(&DEFAULT_ORDERING_COMPONENT)
            .ok_or_else(|| anyhow!("default ordering component not found"))?;

        let coverage_storages = self
            .graph
            .get_all_components(Some(AnnotationComponentType::Coverage), None)
            .into_iter()
            .filter_map(|c| self.graph.get_graphstorage_as_ref(&c))
            .filter(|gs| {
                gs.get_statistics()
                    .map(|stats| stats.nodes > 0)
                    .unwrap_or(true)
            })
            .collect_vec();

        let segmentation_anno_key = AnnoKey {
            ns: DEFAULT_NS.into(),
            name: segmentation.into(),
        };

        let mut segmentation_node_ids = Vec::new();

        let mut next_token_id = ordering_storage
            .root_nodes()
            .at_most_one()
            .map_err(|err| anyhow::Error::msg(err.to_string()))?;

        while let Some(token_id) = next_token_id.take() {
            let token_id = token_id?;

            for coverage_storage in &coverage_storages {
                for covering_node_id in coverage_storage.get_ingoing_edges(token_id) {
                    let covering_node_id = covering_node_id?;

                    if self
                        .graph
                        .get_node_annos()
                        .get_value_for_item(&covering_node_id, &segmentation_anno_key)?
                        .is_some()
                        && !segmentation_node_ids.contains(&covering_node_id)
                    {
                        segmentation_node_ids.push(covering_node_id);
                    }
                }
            }

            next_token_id = ordering_storage
                .get_outgoing_edges(token_id)
                .at_most_one()
                .map_err(|err| anyhow::Error::msg(err.to_string()))?;
        }

        Ok(Nodes {
            graph: &self.graph,
            ids_iter: segmentation_node_ids.into_iter(),
        })
    }
}

pub(crate) struct Nodes<'a> {
    graph: &'a AnnotationGraph,
    ids_iter: vec::IntoIter<NodeID>,
}

impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(Node {
            graph: self.graph,
            id: self.ids_iter.next()?,
        })
    }
}

pub(crate) struct Node<'a> {
    graph: &'a AnnotationGraph,
    id: NodeID,
}

impl<'a> Node<'a> {
    pub(crate) fn anno(&self, anno_key: &AnnoKey) -> anyhow::Result<Option<Cow<'a, str>>> {
        Ok(self
            .graph
            .get_node_annos()
            .get_value_for_item(&self.id, anno_key)?)
    }

    pub(crate) fn name(&self) -> anyhow::Result<NodeName<'a>> {
        Ok(NodeName(self.anno(&NODE_NAME_KEY)?.ok_or_else(|| {
            anyhow!("node {} has no annis:node_name", self.id)
        })?))
    }
}

#[derive(Debug)]
pub(crate) struct NodeName<'a>(Cow<'a, str>);

impl NodeName<'_> {
    pub(crate) fn into_owned_name(self) -> String {
        self.0.into_owned()
    }
}

impl AsRef<str> for NodeName<'_> {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl Display for NodeName<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
