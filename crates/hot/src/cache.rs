use crate::val::Val;

pub fn get_resolved_conf(conf: Val) -> Val {
    conf.merge(&crate::val!({
        "enabled": false,
        "format": "zstdjson"
    }))
}
