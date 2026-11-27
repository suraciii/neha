//! TypeScript representations for wire values that serialize by hand.
//!
//! The record and the surface spell these values as strings, so the generated
//! client type is `string`. ts-rs cannot derive them: they hold a UUID, a
//! sequence number, a timestamp, or a computer kind whose wire spelling is
//! hand-written and sometimes accepts legacy names.

use crate::id::Id;
use crate::record::ComputerKind;
use crate::seq::Seq;
use crate::surface::BotComputer;
use crate::time::Timestamp;

macro_rules! string_ts {
    ($($ty:ty),+ $(,)?) => {$(
        impl ts_rs::TS for $ty {
            type WithoutGenerics = Self;
            type OptionInnerType = Self;

            fn name(_: &ts_rs::Config) -> String {
                "string".to_owned()
            }

            fn inline(cfg: &ts_rs::Config) -> String {
                <Self as ts_rs::TS>::name(cfg)
            }
        }
    )+};
}

string_ts!(Id, Seq, Timestamp, BotComputer, ComputerKind);
