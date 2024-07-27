use std::borrow::Cow;
use std::sync::LazyLock;

use graphannis_core::types::AnnoKey;

pub(crate) const TOK_ANNO: &str = "tok_anno";
pub(crate) const ANNOTATION: &str = "annotation";

pub(crate) static ANNO_KEY_INFLECTION: LazyLock<AnnoKey> = LazyLock::new(|| AnnoKey {
    ns: ANNOTATION.into(),
    name: "inflection".into(),
});

pub(crate) static ANNO_KEY_LEMMA: LazyLock<AnnoKey> = LazyLock::new(|| AnnoKey {
    ns: ANNOTATION.into(),
    name: "lemma".into(),
});

pub(crate) static ANNO_KEY_NORM: LazyLock<AnnoKey> = LazyLock::new(|| AnnoKey {
    ns: ANNOTATION.into(),
    name: "norm".into(),
});

pub(crate) static ANNO_KEY_POS: LazyLock<AnnoKey> = LazyLock::new(|| AnnoKey {
    ns: ANNOTATION.into(),
    name: "pos".into(),
});

pub(crate) fn sanitize_anno(anno: Option<&str>) -> Option<Cow<'_, str>> {
    anno.filter(|&anno| anno != "--").map(str::trim).map(|s| {
        if s.contains('#') {
            Cow::Owned(s.replace('#', "-"))
        } else {
            Cow::Borrowed(s)
        }
    })
}
