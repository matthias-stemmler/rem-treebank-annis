use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::BufReader;
use std::iter::successors;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::{fmt, fs, vec};

use anyhow::{anyhow, bail};
use itertools::Itertools;
use rio_api::model::{Literal, NamedNode, Subject, Term};
use rio_api::parser::TriplesParser;
use rio_turtle::{TurtleError, TurtleParser};
use tracing::{info, warn};

macro_rules! define_named_nodes {
    (
        $(
            $ns:ident = $prefix:literal {
                $($name:ident = $suffix:literal,)*
            },
        )*
    ) => {
        $(
            mod $ns {
                use rio_api::model::NamedNode;

                $(
                    pub(super) const $name: NamedNode<'_> = NamedNode {
                        iri: concat!($prefix, $suffix)
                    };
                )*
            }
        )*
    };
}

define_named_nodes! {
    conll = "http://ufal.mff.cuni.cz/conll2009-st/task-description.html#" {
        CAT = "CAT",
        HEAD = "HEAD",
        INFL = "INFL",
        LEMMA = "LEMMA",
        POS = "POS",
        WORD = "WORD",
    },
    nif = "http://persistence.uni-leipzig.org/nlp2rdf/ontologies/nif-core#" {
        NEXT_SENTENCE = "nextSentence",
        NEXT_WORD = "nextWord",
        SENTENCE = "Sentence",
        WORD = "Word",
    },
    powla = "http://purl.org/powla/powla.owl#" {
        HAS_PARENT = "hasParent",
    },
    rdf = "http://www.w3.org/1999/02/22-rdf-syntax-ns#" {
        TYPE = "type",
    },
}

#[derive(Debug)]
pub(crate) struct Storage {
    dir: PathBuf,
}

impl Storage {
    pub(crate) fn from_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn document_for_name(&self, doc_name: &str) -> anyhow::Result<Option<Document>> {
        let mut doc_path: Option<PathBuf> = None;

        for entry in fs::read_dir(&self.dir)? {
            let file_path = entry?.path();

            if file_path.extension() == Some(OsStr::new("ttl"))
                && file_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with(&format!("{doc_name}_")))
            {
                info!(doc_name, path = %file_path.display(), "found document");

                match doc_path {
                    Some(previous_doc_path) => {
                        bail!(
                            "ttl file path for document {doc_name} is not unique: found at least {}, {}",
                            previous_doc_path.display(),
                            file_path.display()
                        );
                    }
                    None => {
                        doc_path = Some(file_path);
                    }
                }
            }
        }

        Document::from_file(
            &doc_path.ok_or_else(|| anyhow!("ttl file for document {doc_name} not found"))?,
        )
    }
}

#[derive(Debug)]
pub(crate) struct Document {
    node_types: HashMap<NodeName, NodeType>,
    node_annos: HashMap<NodeName, HashMap<AnnoKey, String>>,

    next_sentence: HashMap<NodeName, NodeName>,
    next_word: HashMap<NodeName, NodeName>,
    word_to_sentence: HashMap<NodeName, NodeName>,

    child_to_parent: Vec<(NodeName, NodeName)>,
}

impl Document {
    fn from_file(path: &Path) -> anyhow::Result<Option<Self>> {
        let file = File::open(path)?;
        let mut parser = TurtleParser::new(BufReader::new(file), None);

        let mut node_types: HashMap<NodeName, NodeType> = HashMap::new();
        let mut node_annos: HashMap<NodeName, HashMap<AnnoKey, String>> = HashMap::new();
        let mut next_sentence: HashMap<NodeName, NodeName> = HashMap::new();
        let mut next_word: HashMap<NodeName, NodeName> = HashMap::new();
        let mut word_to_sentence: HashMap<NodeName, NodeName> = HashMap::new();
        let mut child_to_parent = Vec::new();

        let result = parser.parse_all::<ParseError>(&mut |t| {
            for (object, ty) in [
                (nif::SENTENCE, NodeType::Sentence),
                (nif::WORD, NodeType::Word),
            ] {
                if t.predicate == rdf::TYPE && t.object == Term::NamedNode(object) {
                    node_types.insert(t.subject.try_as_named_node()?.node_name(), ty);
                }
            }

            for (predicate, map) in [
                (nif::NEXT_SENTENCE, &mut next_sentence),
                (nif::NEXT_WORD, &mut next_word),
                (conll::HEAD, &mut word_to_sentence),
            ] {
                if t.predicate == predicate {
                    map.insert(
                        t.subject.try_as_named_node()?.node_name(),
                        t.object.try_as_named_node()?.node_name(),
                    );
                }
            }

            if t.predicate == powla::HAS_PARENT {
                child_to_parent.push((
                    t.subject.try_as_named_node()?.node_name(),
                    t.object.try_as_named_node()?.node_name(),
                ));
            }

            for (predicate, anno_key) in [
                (conll::CAT, AnnoKey::Cat),
                (conll::INFL, AnnoKey::Infl),
                (conll::LEMMA, AnnoKey::Lemma),
                (conll::POS, AnnoKey::Pos),
                (conll::WORD, AnnoKey::Word),
            ] {
                if t.predicate == predicate {
                    node_annos
                        .entry(t.subject.try_as_named_node()?.node_name())
                        .or_default()
                        .insert(anno_key, t.object.try_as_simple_literal()?.into());
                }
            }

            Ok(())
        });

        match result {
            Ok(()) => Ok(Some(Self {
                node_types,
                node_annos,
                next_sentence,
                next_word,
                word_to_sentence,
                child_to_parent,
            })),
            Err(ParseError::Anyhow(err)) => Err(err),
            Err(ParseError::Turtle(err)) => {
                warn!(path = %path.display(), %err, "ttl file could not be parsed");
                Ok(None)
            }
        }
    }

    pub(crate) fn word_nodes_in_order(&self) -> Nodes<'_> {
        let sentence_node_names_in_order = successors(
            self.node_names_for_type(NodeType::Sentence)
                .find(|&s| self.next_sentence.values().all(|v| v != s)),
            |&s| self.next_sentence.get(s),
        );

        let word_node_names_in_order = sentence_node_names_in_order
            .flat_map(|s| {
                successors(
                    self.node_names_for_type(NodeType::Word).find(|&w| {
                        self.word_to_sentence.get(w) == Some(s)
                            && self.next_word.values().all(|v| v != w)
                    }),
                    |&w| self.next_word.get(w),
                )
            })
            .collect_vec();

        Nodes {
            document: self,
            names_iter: word_node_names_in_order.into_iter(),
        }
    }

    pub(crate) fn parent_edges(&self) -> impl Iterator<Item = (Node<'_>, Node<'_>)> {
        self.child_to_parent
            .iter()
            .map(|(child, parent)| (self.node_for_name(child), self.node_for_name(parent)))
    }

    fn node_names_for_type(&self, node_type: NodeType) -> impl Iterator<Item = &NodeName> {
        self.node_types
            .iter()
            .filter(move |(_, &t)| t == node_type)
            .map(|(node_name, _)| node_name)
    }

    fn node_for_name<'a>(&'a self, name: &'a NodeName) -> Node<'a> {
        Node {
            document: self,
            name,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Nodes<'a> {
    document: &'a Document,
    names_iter: vec::IntoIter<&'a NodeName>,
}

impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(Node {
            document: self.document,
            name: self.names_iter.next()?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Node<'a> {
    document: &'a Document,
    name: &'a NodeName,
}

impl Node<'_> {
    pub(crate) fn node_name(&self) -> &NodeName {
        self.name
    }

    pub(crate) fn is_word(&self) -> bool {
        self.node_type() == Some(NodeType::Word)
    }

    pub(crate) fn anno(&self, anno_key: AnnoKey) -> Option<&str> {
        self.document
            .node_annos
            .get(self.name)
            .and_then(|annos| annos.get(&anno_key).map(|s| s.deref()))
    }

    fn node_type(&self) -> Option<NodeType> {
        self.document.node_types.get(self.name).copied()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct NodeName(String);

impl AsRef<str> for NodeName {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl Display for NodeName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<NodeName> for String {
    fn from(node_name: NodeName) -> Self {
        node_name.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AnnoKey {
    Cat,
    Infl,
    Lemma,
    Pos,
    Word,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum NodeType {
    Sentence,
    Word,
}

trait NamedNodeExt {
    fn node_name(&self) -> NodeName;
}

impl NamedNodeExt for NamedNode<'_> {
    fn node_name(&self) -> NodeName {
        NodeName(self.iri.into())
    }
}

trait TryAsNamedNode<'a> {
    fn try_as_named_node(&self) -> anyhow::Result<&NamedNode<'a>>;
}

impl<'a> TryAsNamedNode<'a> for Subject<'a> {
    fn try_as_named_node(&self) -> anyhow::Result<&NamedNode<'a>> {
        match self {
            Subject::NamedNode(n) => Ok(n),
            _ => Err(anyhow!("subject {self} is not a NamedNode")),
        }
    }
}

impl<'a> TryAsNamedNode<'a> for Term<'a> {
    fn try_as_named_node(&self) -> anyhow::Result<&NamedNode<'a>> {
        match self {
            Term::NamedNode(n) => Ok(n),
            _ => Err(anyhow!("term {self} is not a named node")),
        }
    }
}

trait TryAsSimpleLiteral<'a> {
    fn try_as_simple_literal(&self) -> anyhow::Result<&'a str>;
}

impl<'a> TryAsSimpleLiteral<'a> for Term<'a> {
    fn try_as_simple_literal(&self) -> anyhow::Result<&'a str> {
        match self {
            Term::Literal(Literal::Simple { value }) => Ok(value),
            _ => Err(anyhow!("term {self} is not a simple literal")),
        }
    }
}

enum ParseError {
    Anyhow(anyhow::Error),
    Turtle(TurtleError),
}

impl From<anyhow::Error> for ParseError {
    fn from(err: anyhow::Error) -> ParseError {
        ParseError::Anyhow(err)
    }
}

impl From<TurtleError> for ParseError {
    fn from(err: TurtleError) -> ParseError {
        ParseError::Turtle(err)
    }
}
