//! `ltoarc tape move`: put cartridges in other slots of their library, or in an I/E slot to
//! take them out. Cartridges that change places go through a free slot.

use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, Result};
use ltoarc_catalog::{LibraryRow, VolumeRow};
use ltoarc_device::{Element, ElementType};

use crate::ctx::Ctx;

/// Where a cartridge is to go: a storage slot, or an import/export slot (None: a free one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Slot(i64),
    Ie(Option<i64>),
}

impl std::str::FromStr for Place {
    type Err = anyhow::Error;

    /// "5", "slot5", "slot 5"; "ie", "ie2", "I/E 2", "mailslot".
    fn from_str(s: &str) -> Result<Place> {
        let t = s.trim().to_ascii_lowercase().replace([' ', '/'], "");
        let num = |n: &str| n.parse::<i64>().ok().filter(|n| *n >= 1);
        let p = if let Some(n) = t.strip_prefix("slot") {
            num(n).map(Place::Slot)
        } else if let Some(n) = t.strip_prefix("mailslot").or_else(|| t.strip_prefix("ie")) {
            if n.is_empty() { Some(Place::Ie(None)) } else { num(n).map(|n| Place::Ie(Some(n))) }
        } else {
            num(&t).map(Place::Slot)
        };
        p.ok_or_else(|| anyhow!("{s}: expected a slot number (5, slot5), or ie / ie2 for an I/E slot"))
    }
}

impl Place {
    /// As `tape move` takes it.
    pub fn arg(&self) -> String {
        match self {
            Place::Slot(n) => n.to_string(),
            Place::Ie(None) => "ie".into(),
            Place::Ie(Some(n)) => format!("ie{n}"),
        }
    }
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Place::Slot(n) => write!(f, "slot {n}"),
            Place::Ie(None) => f.write_str("an I/E slot"),
            Place::Ie(Some(n)) => write!(f, "I/E {n}"),
        }
    }
}

/// `BARCODE PLACE`, or any number of `BARCODE=PLACE`.
pub fn parse_moves(args: &[String]) -> Result<Vec<(String, Place)>> {
    if let [bc, place] = args {
        if !bc.contains('=') && !place.contains('=') {
            return Ok(vec![(bc.clone(), place.parse()?)]);
        }
    }
    args.iter()
        .map(|a| {
            // The place has no '='; a name given to a cartridge without barcode may.
            let (bc, place) = a.rsplit_once('=').ok_or_else(|| anyhow!("{a}: expected BARCODE=PLACE (or a single BARCODE PLACE)"))?;
            Ok((bc.trim().to_string(), place.parse()?))
        })
        .collect()
}

/// Robot moves (from, to) that take each cartridge of `wants` from its element to the one
/// wanted. `full`: the elements holding cartridges; a wanted element that is full must be
/// emptied by another of `wants`. Cartridges that wait on each other (they change places)
/// make room through the first of `spare` that is empty; None when there is none.
pub fn plan(spare: &[u16], full: &BTreeSet<u16>, wants: &[(u16, u16)]) -> Option<Vec<(u16, u16)>> {
    let mut full = full.clone();
    let mut pending: Vec<(u16, u16)> = wants.iter().copied().filter(|(f, t)| f != t).collect();
    let mut moves = Vec::new();
    while !pending.is_empty() {
        let Some(i) = pending.iter().position(|(_, t)| !full.contains(t)) else {
            // Each one's place holds another of them (so no empty slot is wanted): put one aside.
            let aside = *spare.iter().find(|e| !full.contains(e))?;
            let from = pending[0].0;
            moves.push((from, aside));
            full.remove(&from);
            full.insert(aside);
            pending[0].0 = aside;
            continue;
        };
        let (from, to) = pending.remove(i);
        moves.push((from, to));
        full.remove(&from);
        full.insert(to);
    }
    Some(moves)
}

pub fn run(ctx: &Ctx, args: &[String], swap: bool, dry_run: bool) -> Result<()> {
    // The cartridges by library, found where they are now.
    let mut groups: Vec<(LibraryRow, Vec<(VolumeRow, Place)>)> = Vec::new();
    for (bc, place) in parse_moves(args)? {
        let (v, lib, ..) = crate::tape::locate(ctx, &bc)?;
        match groups.iter_mut().find(|(l, _)| l.id == lib.id) {
            Some((_, list)) => list.push((v, place)),
            None => groups.push((lib, vec![(v, place)])),
        }
    }
    for (lib, list) in &groups {
        arrange(ctx, lib, list, swap, dry_run)?;
    }
    if dry_run {
        println!("dry run; nothing moved");
    }
    Ok(())
}

fn shelf(e: &Element) -> bool {
    matches!(e.kind, ElementType::Storage | ElementType::ImportExport)
}

fn arrange(ctx: &Ctx, lib: &LibraryRow, list: &[(VolumeRow, Place)], swap: bool, dry_run: bool) -> Result<()> {
    // Hold the robot from reading the inventory to the last move.
    let _arm = ctx.leases.changer(lib)?;
    let mut changer = ctx.open_changer(lib)?;
    let inv = changer.inventory()?;
    let name = |a: u16| lib.element_name(a as i64);
    let full: BTreeSet<u16> = inv.elements.iter().filter(|e| shelf(e) && e.full).map(|e| e.address).collect();
    // The cartridges to move (name, catalog entry) by the element they are in.
    let mut who: HashMap<u16, (String, Option<VolumeRow>)> = HashMap::new();
    let mut wants: Vec<(u16, u16)> = Vec::new();
    for (v, place) in list {
        let el = ctx.find_element(v, &inv).ok_or_else(|| anyhow!("{} is not in {} any more; run `ltoarc tape register`", v.barcode, lib.name))?;
        if !shelf(&el) {
            bail!("{} is in a drive; `ltoarc tape unload --slot N` puts it in a slot", v.barcode);
        }
        let wanted = |a: u16| wants.iter().any(|w| w.1 == a);
        let to = match *place {
            Place::Slot(n) => lib.slot_address(n).ok_or_else(|| anyhow!("{} has no slot {n}", lib.name))? as u16,
            Place::Ie(Some(n)) => lib.ie_address(n).ok_or_else(|| anyhow!("{} has no I/E slot {n}", lib.name))? as u16,
            Place::Ie(None) if el.kind == ElementType::ImportExport => el.address,
            Place::Ie(None) => inv
                .of(ElementType::ImportExport)
                .find(|e| !e.full && e.exception.is_none() && !wanted(e.address))
                .map(|e| e.address)
                .ok_or_else(|| anyhow!("{} has no free I/E slot", lib.name))?,
        };
        if who.contains_key(&el.address) {
            bail!("{} is named twice", v.barcode);
        }
        if wanted(to) {
            bail!("two cartridges are to go to {} {}", lib.name, name(to));
        }
        who.insert(el.address, (v.barcode.clone(), Some(v.clone())));
        wants.push((el.address, to));
    }
    // A place that holds a cartridge not moving away: with --swap, that one takes the place
    // of the cartridge moving in.
    let mut displaced = Vec::new();
    for &(from, to) in &wants {
        if !full.contains(&to) || who.contains_key(&to) {
            continue;
        }
        let (holder, v) = match inv.element(to).and_then(|e| e.barcode.clone()) {
            Some(b) => (b.clone(), ctx.cat.volume(&b)?),
            None => match ctx.cat.unlabelled_at(lib.id, to as i64)? {
                Some(v) => (v.barcode.clone(), Some(v)),
                None => ("a cartridge without barcode".to_string(), None),
            },
        };
        if !swap {
            let say = if v.is_some() { format!(", or say where it goes ({holder}=PLACE)") } else { String::new() };
            bail!("{} {} holds {holder}; add --swap to exchange them{say}", lib.name, name(to));
        }
        if wants.iter().any(|w| w.1 == from) {
            bail!("{holder} cannot take the place of {}: another cartridge goes to {}", who[&from].0, name(from));
        }
        displaced.push((to, from));
        who.insert(to, (holder, v));
    }
    wants.extend(displaced);
    for &(at, _) in wants.iter().filter(|(f, t)| f == t) {
        println!("{} is in {} {} already", who[&at].0, lib.name, name(at));
    }
    let spare: Vec<u16> =
        inv.of(ElementType::Storage).chain(inv.of(ElementType::ImportExport)).filter(|e| e.exception.is_none()).map(|e| e.address).collect();
    let moves = plan(&spare, &full, &wants).ok_or_else(|| anyhow!("{} has no free slot to exchange cartridges through", lib.name))?;
    for (from, to) in moves {
        let (bc, v) = who.remove(&from).expect("a cartridge to move");
        if dry_run {
            println!("would move {bc} from {} {} to {}", lib.name, name(from), name(to));
        } else {
            changer.move_medium(from, to)?;
            if let Some(v) = &v {
                let mut v = ctx.volume(&v.barcode)?;
                v.library_id = Some(lib.id);
                v.element = Some(to as i64);
                ctx.cat.update_volume(&v)?;
                ctx.cat.log_event("tape", &v.barcode, &format!("moved from {} to {}", name(from), name(to)))?;
            }
            println!("{bc} moved from {} {} to {}", lib.name, name(from), name(to));
        }
        who.insert(to, (bc, v));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(a: &[u16]) -> BTreeSet<u16> {
        a.iter().copied().collect()
    }

    /// Runs the moves on `full` (checking each one), and returns what ends where.
    fn apply(full: &[u16], moves: &[(u16, u16)]) -> BTreeSet<u16> {
        let mut f = set(full);
        for (a, b) in moves {
            assert!(f.remove(a), "{a} is empty: {moves:?}");
            assert!(f.insert(*b), "{b} is full: {moves:?}");
        }
        f
    }

    #[test]
    fn places() {
        assert_eq!("5".parse::<Place>().unwrap(), Place::Slot(5));
        assert_eq!("Slot 12".parse::<Place>().unwrap(), Place::Slot(12));
        assert_eq!("ie".parse::<Place>().unwrap(), Place::Ie(None));
        assert_eq!("I/E 2".parse::<Place>().unwrap(), Place::Ie(Some(2)));
        assert_eq!("mailslot".parse::<Place>().unwrap(), Place::Ie(None));
        assert!("0".parse::<Place>().is_err());
        assert!("drive1".parse::<Place>().is_err());
        let one = parse_moves(&["DC8001L8".into(), "7".into()]).unwrap();
        assert_eq!(one, [("DC8001L8".to_string(), Place::Slot(7))]);
        let many = parse_moves(&["A=1".into(), "B=ie".into(), "X=Y=3".into()]).unwrap();
        assert_eq!(many, [("A".into(), Place::Slot(1)), ("B".into(), Place::Ie(None)), ("X=Y".into(), Place::Slot(3))]);
        assert!(parse_moves(&["A".into(), "B".into(), "C".into()]).is_err());
    }

    #[test]
    fn moves_in_order_and_through_a_free_slot() {
        let spare = [1, 2, 3, 4, 5];
        // To an empty slot.
        assert_eq!(plan(&spare, &set(&[1]), &[(1, 3)]).unwrap(), [(1, 3)]);
        // A chain: 2 leaves for 3 before 1 takes its place.
        let m = plan(&spare, &set(&[1, 2]), &[(1, 2), (2, 3)]).unwrap();
        assert_eq!(m, [(2, 3), (1, 2)]);
        // Two that change places go through the first free slot.
        let m = plan(&spare, &set(&[1, 2]), &[(1, 2), (2, 1)]).unwrap();
        assert_eq!(m, [(1, 3), (2, 1), (3, 2)]);
        // A three-way turn, with a slot wanted by none.
        let m = plan(&spare, &set(&[1, 2, 3]), &[(1, 2), (2, 3), (3, 1)]).unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!(apply(&[1, 2, 3], &m), set(&[1, 2, 3]));
        // Moves that can go first do, and a slot one of them leaves makes room.
        let m = plan(&spare, &set(&[1, 2, 3]), &[(1, 2), (2, 1), (3, 4)]).unwrap();
        assert_eq!(m, [(3, 4), (1, 3), (2, 1), (3, 2)]);
        // Staying put is no move.
        assert_eq!(plan(&spare, &set(&[1]), &[(1, 1)]).unwrap(), []);
        // Nowhere to make room.
        assert_eq!(plan(&[1, 2], &set(&[1, 2]), &[(1, 2), (2, 1)]), None);
    }
}
