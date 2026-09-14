//! Parse Kitty graphics control data (comma-separated `key=value`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Transmit,
    Query,
    Delete,
    /// `a=p` put/display without a new transmit.
    Put,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteKind {
    All,
    ImageId,
    ImageNumber,
    Cursor,
    Position,
    PositionZ,
    Column,
    Row,
    Z,
    Range,
    Frame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeleteMode {
    pub kind: DeleteKind,
    pub free: bool,
}

impl DeleteMode {
    fn parse(value: &str) -> Option<Self> {
        let free = value.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        let kind = match value.to_ascii_lowercase().as_str() {
            "a" => DeleteKind::All,
            "i" => DeleteKind::ImageId,
            "n" => DeleteKind::ImageNumber,
            "c" => DeleteKind::Cursor,
            "p" => DeleteKind::Position,
            "q" => DeleteKind::PositionZ,
            "x" => DeleteKind::Column,
            "y" => DeleteKind::Row,
            "z" => DeleteKind::Z,
            "r" => DeleteKind::Range,
            "f" => DeleteKind::Frame,
            _ => return None,
        };
        Some(Self { kind, free })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    Direct,
    File,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GraphicsCommand {
    pub action: Action,
    pub delete_mode: Option<DeleteMode>,
    pub format: u16,
    pub transport: Transport,
    pub more: bool,
    pub id: u32,
    pub id_present: bool,
    pub image_number: Option<u32>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub z: Option<i32>,
    pub cols: u16,
    pub rows: u16,
    pub quiet: u8,
    /// A control without `a=` is a continuation chunk.
    pub continuation: bool,
    /// `a=T` (cursor-place unless `unicode_placement`). `a=t` is false.
    pub place: bool,
    /// `U=1` — virtual placement for Unicode placeholders.
    pub unicode_placement: bool,
    /// `p=` placement id (underline color in the grid).
    pub placement_id: u32,
    pub placement_id_present: bool,
}

pub(crate) fn parse(control: &str) -> GraphicsCommand {
    let mut cmd = GraphicsCommand {
        action: Action::Other,
        delete_mode: None,
        format: 32,
        transport: Transport::Direct,
        more: false,
        id: 0,
        id_present: false,
        image_number: None,
        x: None,
        y: None,
        z: None,
        cols: 0,
        rows: 0,
        quiet: 0,
        continuation: false,
        place: false,
        unicode_placement: false,
        placement_id: 0,
        placement_id_present: false,
    };
    // Kitty omits `a=` for continuation chunks; a bare control still parses.
    let mut saw_action = false;
    for pair in control.split(',') {
        let Some((key, val)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "a" => {
                saw_action = true;
                match val {
                    "T" => {
                        cmd.action = Action::Transmit;
                        cmd.place = true;
                    }
                    "t" => {
                        cmd.action = Action::Transmit;
                        cmd.place = false;
                    }
                    "q" => cmd.action = Action::Query,
                    "d" => cmd.action = Action::Delete,
                    "p" => cmd.action = Action::Put,
                    _ => cmd.action = Action::Other,
                }
            }
            "d" => cmd.delete_mode = DeleteMode::parse(val),
            "t" => {
                cmd.transport = match val {
                    "d" => Transport::Direct,
                    "f" => Transport::File,
                    _ => Transport::Other,
                };
            }
            "f" => cmd.format = val.parse().unwrap_or(cmd.format),
            "m" => cmd.more = val == "1",
            "i" => {
                cmd.id_present = true;
                cmd.id = val.parse().unwrap_or(0);
            }
            "I" => cmd.image_number = val.parse().ok(),
            "x" => cmd.x = val.parse().ok(),
            "y" => cmd.y = val.parse().ok(),
            "z" => cmd.z = val.parse().ok(),
            "c" => cmd.cols = val.parse().unwrap_or(0),
            "r" => cmd.rows = val.parse().unwrap_or(0),
            "q" => cmd.quiet = val.parse().unwrap_or(0),
            "U" => cmd.unicode_placement = val != "0",
            "p" => {
                cmd.placement_id_present = true;
                cmd.placement_id = val.parse().unwrap_or(0);
            }
            _ => {}
        }
    }
    // A continuation chunk (no `a=`) is a Transmit continuation by convention.
    if !saw_action {
        cmd.action = Action::Transmit;
        cmd.continuation = true;
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transmit_display() {
        let c = parse("a=T,t=d,f=100,q=2");
        assert_eq!(c.action, Action::Transmit);
        assert_eq!(c.transport, Transport::Direct);
        assert_eq!(c.format, 100);
        assert_eq!(c.quiet, 2);
        assert!(!c.more);
    }

    #[test]
    fn parses_query_and_file_and_chunk() {
        assert_eq!(parse("i=31,a=q,t=d,f=24").action, Action::Query);
        assert_eq!(parse("a=T,t=f,f=100,i=7").transport, Transport::File);
        assert!(parse("a=T,t=d,f=100,m=1").more);
        assert_eq!(parse("i=42").id, 42);
        assert!(parse("i=42").id_present);
        assert!(!parse("I=42").id_present);
        assert!(parse("m=0").continuation);
    }

    #[test]
    fn parses_delete_matrix_controls() {
        let cases = [
            ("a=d,d=a", DeleteKind::All, false),
            ("a=d,d=A", DeleteKind::All, true),
            ("a=d,d=i,i=7,p=3", DeleteKind::ImageId, false),
            ("a=d,d=I,i=7", DeleteKind::ImageId, true),
            ("a=d,d=n,I=42", DeleteKind::ImageNumber, false),
            ("a=d,d=N,I=42,p=3", DeleteKind::ImageNumber, true),
            ("a=d,d=c", DeleteKind::Cursor, false),
            ("a=d,d=p,x=3,y=4", DeleteKind::Position, false),
            ("a=d,d=Q,x=3,y=4,z=-1", DeleteKind::PositionZ, true),
            ("a=d,d=x,x=3", DeleteKind::Column, false),
            ("a=d,d=Y,y=4", DeleteKind::Row, true),
            ("a=d,d=z,z=-1", DeleteKind::Z, false),
            ("a=d,d=r,x=3,y=9", DeleteKind::Range, false),
            ("a=d,d=F", DeleteKind::Frame, true),
        ];
        for (control, kind, free) in cases {
            let command = parse(control);
            assert_eq!(
                command.delete_mode,
                Some(DeleteMode { kind, free }),
                "{control}"
            );
        }
        let command = parse("a=d,d=q,x=3,y=4,z=-1,I=8,p=2");
        assert_eq!(command.image_number, Some(8));
        assert_eq!(command.x, Some(3));
        assert_eq!(command.y, Some(4));
        assert_eq!(command.z, Some(-1));
        assert_eq!(command.placement_id, 2);
        assert!(command.placement_id_present);
        assert_eq!(parse("a=d,d=x,x=bad").x, None);
        assert_eq!(parse("a=d,d=unknown").delete_mode, None);
    }

    #[test]
    fn ignores_unknown_keys_and_defaults() {
        let c = parse("a=T,zz=99");
        assert_eq!(c.format, 32);
        assert_eq!(c.transport, Transport::Direct);
        assert_eq!(c.action, Action::Transmit);
    }

    #[test]
    fn unknown_action_is_other() {
        assert_eq!(parse("a=X").action, Action::Other);
        assert_eq!(parse("t=s").transport, Transport::Other);
    }

    #[test]
    fn parses_transmit_only_and_put_unicode() {
        let t = parse("a=t,t=d,f=100,i=7");
        assert_eq!(t.action, Action::Transmit);
        assert!(!t.place);
        let p = parse("a=p,U=1,i=7,c=2,r=1,p=3");
        assert_eq!(p.action, Action::Put);
        assert!(p.unicode_placement);
        assert_eq!(p.id, 7);
        assert_eq!(p.cols, 2);
        assert_eq!(p.rows, 1);
        assert_eq!(p.placement_id, 3);
        let tu = parse("a=T,U=1,i=9,c=4,r=2");
        assert_eq!(tu.action, Action::Transmit);
        assert!(tu.place);
        assert!(tu.unicode_placement);
    }
}
