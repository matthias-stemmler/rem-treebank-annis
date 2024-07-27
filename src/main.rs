use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{anyhow, bail, ensure};
use clap::Parser;
use itertools::{EitherOrBoth, Itertools};
use tracing::{error, info};

mod annis_util;
mod rem;

mod inbound {
    pub(crate) mod annis;
    pub(crate) mod ttl;
}

mod outbound {
    pub(crate) mod annis;
}

/// Converts the Treebank edition of the Referenzkorpus Mittelhochdeutsch (ReM) into the ANNIS
/// format
#[derive(Parser)]
struct Args {
    /// Path to input corpora, must be a .zip file containing the ReM in the relANNIS or GraphML
    /// format
    #[arg(value_name = "INPUT ANNIS ZIP")]
    input_annis: PathBuf,

    /// Path to input treebank data, must be a directory containing the treebank data in the Turtle
    /// (.ttl) format
    #[arg(value_name = "INPUT TTL DIRECTORY")]
    input_ttl: PathBuf,

    /// Path to output corpus, will be a .zip file containing the merged corpus in the
    /// GraphML format [default: like input corpus, but with `.out.zip` extension]
    #[arg(long, value_name = "ANNIS ZIP")]
    output: Option<PathBuf>,

    /// If specified, rename corpora using this pattern
    /// Must contain the placeholder `%c` representing the original corpus name, e.g. `%c_treebank`
    /// This facilitates importing the original and new corpora into the same ANNIS data directory
    #[arg(long, value_name = "PATTERN")]
    rename: Option<RenamePattern>,

    /// Layer (namespace) of the treebank nodes
    #[arg(long, default_value = "treebank", value_name = "TREE LAYER")]
    layer: String,

    /// Name of the treebank annotation
    #[arg(long, default_value = "tree", value_name = "TREE ANNO")]
    tree_anno: String,

    /// Display name for the ANNIS tree visualizer
    #[arg(long, default_value = "tree", value_name = "TREE DISPLAY")]
    tree_display: String,

    /// If specified, add an annotation of this name to each node containg the IRI of the
    /// corresponding TTL node where applicable
    #[arg(long, value_name = "IRI ANNO")]
    iri_anno: Option<String>,

    /// Whether to store temporary ANNIS corpus graphs in memory rather than on disk.
    /// Running with this flag is faster, but can fail if there is not enough memory to fit the
    /// corpus graphs.
    #[arg(long, default_value = "false")]
    in_memory: bool,
}

#[derive(Clone)]
struct RenamePattern(String);

impl FromStr for RenamePattern {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains("%c") {
            Ok(Self(s.into()))
        } else {
            bail!("pattern must contain placeholder `%c`");
        }
    }
}

impl RenamePattern {
    fn apply(&self, name: &str) -> String {
        self.0.replace("%c", name)
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    if let Err(err) = run() {
        error!("{}", err);
    }
}

fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let annis_storage = inbound::annis::Storage::from_zip(&args.input_annis, args.in_memory)?;
    let ttl_storage = inbound::ttl::Storage::from_dir(args.input_ttl);

    let output_path = args
        .output
        .unwrap_or_else(|| match args.input_annis.file_stem() {
            Some(stem) => {
                let mut file_name = stem.to_os_string();
                file_name.push(".out.zip");
                args.input_annis.with_file_name(&file_name)
            }
            None => PathBuf::from("out.zip"),
        });

    let mut corpus_writer = outbound::annis::CorpusWriter::new(&output_path)?;

    for inbound_corpus in annis_storage.corpora() {
        info!(corpus_name = inbound_corpus.name(), "processing corpus");

        let mut outbound_corpus = outbound::annis::Corpus::from_inbound_corpus(&inbound_corpus);
        let mut update = outbound_corpus.begin_update();

        for annis_doc in inbound_corpus.documents()? {
            let annis_doc = annis_doc?;
            let doc_name = annis_doc.doc_name()?;

            let Some(ttl_doc) = ttl_storage.document_for_name(doc_name)? else {
                info!(doc_name, "skipping document");
                continue;
            };

            info!(doc_name, "processing document");

            let node_name_mapper = NodeNameMapper::new(&ttl_doc, &annis_doc)?;

            // Add all edges that are reachable from words
            let mut ttl_node_names: HashSet<inbound::ttl::NodeName> = HashSet::new();
            let mut parent_edges = Some(ttl_doc.parent_edges().collect_vec());

            while let Some(edges) = parent_edges.take() {
                let mut remaining_edges = Vec::with_capacity(edges.len());
                let mut added_edge = false;

                for (child, parent) in edges {
                    if child.is_word() || ttl_node_names.contains(child.node_name()) {
                        // skip sentence roots, which have no `CAT` annotation
                        if parent.anno(inbound::ttl::AnnoKey::Cat).is_none() {
                            continue;
                        }

                        for ttl_node in [child, parent] {
                            if ttl_node_names.insert(ttl_node.node_name().clone()) {
                                let annis_node_name = node_name_mapper.annis_node_name(ttl_node)?;

                                if !ttl_node.is_word() {
                                    update.add_node(
                                        annis_node_name.clone(),
                                        outbound::annis::NODE.into(),
                                    )?;

                                    // annis:layer = <layer>
                                    update.add_node_anno(
                                        annis_node_name.clone(),
                                        outbound::annis::ANNIS_NS.into(),
                                        outbound::annis::LAYER.into(),
                                        args.layer.clone(),
                                    )?;

                                    // <layer>:<tree_anno> = <cat>
                                    if let Some(cat) = ttl_node.anno(inbound::ttl::AnnoKey::Cat) {
                                        update.add_node_anno(
                                            annis_node_name.clone(),
                                            args.layer.clone(),
                                            args.tree_anno.clone(),
                                            cat.into(),
                                        )?;
                                    }
                                }

                                if let Some(iri_anno) = &args.iri_anno {
                                    // <layer>:<iri_anno> = <iri>
                                    update.add_node_anno(
                                        annis_node_name.clone(),
                                        args.layer.clone(),
                                        iri_anno.into(),
                                        ttl_node.node_name().clone().into(),
                                    )?;
                                }
                            }
                        }

                        // Dominance/<layer>/ from parent to child
                        update.add_edge(
                            node_name_mapper.annis_node_name(parent)?,
                            node_name_mapper.annis_node_name(child)?,
                            &outbound::annis::AnnotationComponentType::Dominance,
                            args.layer.clone(),
                            "".into(),
                        )?;

                        added_edge = true;
                    } else {
                        remaining_edges.push((child, parent));
                    }
                }

                if added_edge {
                    parent_edges = Some(remaining_edges);
                }
            }
        }

        update.apply()?;

        let mut update = outbound_corpus.begin_update();

        for m in outbound_corpus.query(&format!(
            "annis:layer=\"{}\" >* node @* annis:node_type=\"datasource\"",
            args.layer
        ))? {
            let [layer_node_name, _, datasource_node_name] = m
                .try_into()
                .map_err(|_| anyhow!("unexpected number of nodes in query match"))?;

            // PartOf/annis/ from node to datasource
            update.add_edge(
                layer_node_name,
                datasource_node_name,
                &outbound::annis::AnnotationComponentType::PartOf,
                outbound::annis::ANNIS_NS.into(),
                "".into(),
            )?;
        }

        update.apply()?;

        if let Some(rename_pattern) = &args.rename {
            outbound_corpus.update_name(|n| rename_pattern.apply(n))?;
        }

        let config = {
            let mut config = inbound_corpus.config()?;

            let visualizers = config
                .entry("visualizers")
                .or_insert_with(|| toml::value::Array::new().into())
                .as_array_mut()
                .ok_or_else(|| anyhow!("invalid corpus config: `visualizers` is not an array"))?;

            visualizers.push({
                let entries: [(String, toml::Value); 6] = [
                    ("display_name".into(), args.tree_display.as_str().into()),
                    ("element".into(), "node".into()),
                    ("layer".into(), args.layer.as_str().into()),
                    ("vis_type".into(), "tree".into()),
                    ("visibility".into(), "hidden".into()),
                    ("mappings".into(), {
                        let entries = [
                            ("edge_type".into(), "null".into()),
                            ("node_anno_ns".into(), args.layer.as_str().into()),
                            ("node_key".into(), args.tree_anno.as_str().into()),
                            ("terminal_ns".into(), outbound::annis::DEFAULT_NS.into()),
                            ("terminal_name".into(), rem::TOK_ANNO.into()),
                        ];
                        entries.into_iter().collect::<toml::Table>().into()
                    }),
                ];
                entries.into_iter().collect::<toml::Table>().into()
            });

            config
        };

        corpus_writer.write_corpus(&outbound_corpus, &config)?;
    }

    corpus_writer.finish()?;

    Ok(())
}

#[derive(Debug)]
struct NodeNameMapper<'a> {
    annis_doc_node_name: String,
    mapping: HashMap<inbound::ttl::NodeName, inbound::annis::NodeName<'a>>,
}

impl<'a> NodeNameMapper<'a> {
    fn new(
        ttl_doc: &inbound::ttl::Document,
        annis_doc: &'a inbound::annis::Document,
    ) -> anyhow::Result<Self> {
        let ttl_nodes = ttl_doc.word_nodes_in_order();
        let annis_nodes = annis_doc.segmentation_nodes_in_order(rem::TOK_ANNO)?;

        let mut mapping = HashMap::new();

        for pair in ttl_nodes.zip_longest(annis_nodes) {
            match pair {
                EitherOrBoth::Both(ttl_node, annis_node) => {
                    let ttl_node_name = ttl_node.node_name().clone();
                    let annis_node_name = annis_node.name()?;

                    // Sanity check: compare common annotations to make sure that mapping is correct
                    for (ttl_anno_key, annis_anno_key) in [
                        (inbound::ttl::AnnoKey::Infl, &rem::ANNO_KEY_INFLECTION),
                        (inbound::ttl::AnnoKey::Lemma, &rem::ANNO_KEY_LEMMA),
                        (inbound::ttl::AnnoKey::Word, &rem::ANNO_KEY_NORM),
                        (inbound::ttl::AnnoKey::Pos, &rem::ANNO_KEY_POS),
                    ] {
                        let ttl_anno = ttl_node
                            .anno(ttl_anno_key)
                            .map(|s| s.replace("&quot;", "\""));
                        let annis_anno = annis_node.anno(annis_anno_key)?;
                        let annis_anno = rem::sanitize_anno(annis_anno.as_deref());

                        ensure!(
                            ttl_anno.as_deref() == annis_anno.as_deref(),
                            "sanity check failed: {} for {} and {} doesn't match: '{}' != '{}'",
                            annis_anno_key.name,
                            ttl_node.node_name(),
                            annis_node.name()?,
                            ttl_anno.as_deref().unwrap_or(""),
                            annis_anno.as_deref().unwrap_or(""),
                        );
                    }

                    mapping.insert(ttl_node_name, annis_node_name);
                }
                EitherOrBoth::Left(ttl_node) => {
                    bail!(
                        "ttl node {} has no counterpart in ANNIS",
                        ttl_node.node_name()
                    )
                }
                EitherOrBoth::Right(_) => {
                    // Ok, since there may be incomplete sentences in ANNIS, which have no
                    // counterpart in TTL
                }
            }
        }

        Ok(Self {
            annis_doc_node_name: annis_doc.node_name().into_owned_name(),
            mapping,
        })
    }

    fn annis_node_name(&self, ttl_node: inbound::ttl::Node<'_>) -> anyhow::Result<String> {
        let ttl_node_name = ttl_node.node_name();

        let annis_node_name = if ttl_node.is_word() {
            self.mapping
                .get(ttl_node_name)
                .ok_or_else(|| anyhow!("missing mapping for ttl node name {ttl_node_name}"))?
                .as_ref()
                .into()
        } else {
            let (_, final_part) = ttl_node_name
                .as_ref()
                .rsplit_once('/')
                .ok_or_else(|| anyhow!("ttl node name contains no '/'"))?;

            format!("{}#{}", self.annis_doc_node_name, final_part)
        };

        Ok(annis_node_name)
    }
}
