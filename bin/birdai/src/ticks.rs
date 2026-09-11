//! Loading a Cetus pool's tick skip list over RPC.
//!
//! The nodes are dynamic fields of the skip list's **inner UID**, reachable only by enumerating
//! that UID's children — enumerating the pool object's own children returns nothing, which is the
//! single easiest way to get this wrong. Each field is an object of type
//! `0x2::dynamic_field::Field<u64, skip_list::Node<tick::Tick>>` whose `value` is the node.

use std::sync::Arc;

use birdai_move::decode_struct;
use birdai_resolve::{LayoutSource, ObjectSource};
use birdai_tick::{FieldNodeDecoder, SkipListHead, TickNode};
use move_core_types::annotated_value::MoveTypeLayout;
use sui_types::object::Object;

/// How many child objects to fetch per batch.
const FETCH_BATCH: usize = 200;

/// Every node of a pool's tick skip list.
///
/// The caller feeds the result to [`birdai_tick::Ticks::new`], which sorts them, asserts the skip
/// list's declared `size` against the number decoded, and checks every node's stored `sqrt_price`
/// against the tick math. A partial enumeration is therefore a hard error rather than a quietly
/// short index.
pub(crate) async fn load_tick_nodes<O, L>(
    source: &O,
    layouts: &L,
    head: &SkipListHead,
    mut on_object: impl FnMut(&Object),
) -> eyre::Result<Vec<TickNode>>
where
    O: ObjectSource + ?Sized,
    L: LayoutSource + ?Sized,
{
    let mut field_ids = Vec::with_capacity(usize::try_from(head.size).unwrap_or(0));
    let mut cursor = None;
    loop {
        let page = source.dynamic_fields(head.node_uid, cursor.clone()).await?;
        if page.entries.is_empty() {
            break;
        }
        field_ids.extend(page.entries.iter().map(|entry| entry.field_id));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    let mut nodes = Vec::with_capacity(field_ids.len());
    let mut layout: Option<Arc<MoveTypeLayout>> = None;
    for chunk in field_ids.chunks(FETCH_BATCH) {
        let objects = source.objects(chunk).await?;
        for object in &objects {
            on_object(object);
            let tag = object
                .struct_tag()
                .ok_or_else(|| eyre::eyre!("dynamic field {} has no type", object.id()))?;
            // Every node shares one type, so the layout is resolved once.
            let layout = if let Some(cached) = &layout {
                cached.clone()
            } else {
                let resolved = layouts.layout(&tag).await?;
                layout = Some(resolved.clone());
                resolved
            };
            let MoveTypeLayout::Struct(struct_layout) = layout.as_ref() else {
                return Err(eyre::eyre!("dynamic field {} has a non-struct layout", object.id()));
            };
            let contents = object
                .data
                .try_as_move()
                .ok_or_else(|| eyre::eyre!("dynamic field {} has no contents", object.id()))?
                .contents();
            nodes.push(decode_struct(contents, struct_layout, FieldNodeDecoder)?);
        }
    }

    Ok(nodes)
}
