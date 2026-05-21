use std::io::{self, Write};

fn write_json_string(out: &mut impl Write, value: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    for byte in value.bytes() {
        match byte {
            b'"' => out.write_all(br#"\""#)?,
            b'\\' => out.write_all(br#"\\"#)?,
            b'\n' => out.write_all(br#"\n"#)?,
            b'\r' => out.write_all(br#"\r"#)?,
            b'\t' => out.write_all(br#"\t"#)?,
            0x20..=0x7e => out.write_all(&[byte])?,
            other => write!(out, "\\u{other:04x}")?,
        }
    }
    out.write_all(b"\"")
}

fn main() -> io::Result<()> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    writeln!(out, "{{\"layouts\":[")?;
    for (layout_idx, layout) in tx_shims::linux_syscall::kernel_user_layouts()
        .iter()
        .enumerate()
    {
        if layout_idx > 0 {
            writeln!(out, ",")?;
        }
        write!(out, "  {{\"rust_type\":")?;
        write_json_string(&mut out, layout.rust_type)?;
        write!(out, ",\"musl_header\":")?;
        write_json_string(&mut out, layout.musl_header)?;
        write!(out, ",\"musl_type\":")?;
        write_json_string(&mut out, layout.musl_type)?;
        write!(
            out,
            ",\"size\":{},\"align\":{},\"fields\":[",
            layout.size, layout.align
        )?;
        for (field_idx, field) in layout.fields.iter().enumerate() {
            if field_idx > 0 {
                write!(out, ",")?;
            }
            write!(out, "{{\"rust\":")?;
            write_json_string(&mut out, field.rust)?;
            write!(out, ",\"musl\":")?;
            write_json_string(&mut out, field.musl)?;
            write!(out, ",\"offset\":{}}}", field.offset)?;
        }
        write!(out, "]}}")?;
    }
    writeln!(out, "\n],\"candidates\":[")?;
    for (candidate_idx, candidate) in tx_shims::linux_syscall::kernel_user_layout_candidates()
        .iter()
        .enumerate()
    {
        if candidate_idx > 0 {
            writeln!(out, ",")?;
        }
        write!(out, "  {{\"name\":")?;
        write_json_string(&mut out, candidate.name)?;
        write!(out, ",\"rust_type\":")?;
        write_json_string(&mut out, candidate.rust_type)?;
        write!(out, ",\"kind\":")?;
        write_json_string(&mut out, candidate.kind)?;
        write!(out, ",\"status\":")?;
        write_json_string(&mut out, candidate.status)?;
        write!(out, ",\"musl_header\":")?;
        write_json_string(&mut out, candidate.musl_header)?;
        write!(out, ",\"musl_type\":")?;
        write_json_string(&mut out, candidate.musl_type)?;
        write!(
            out,
            ",\"size\":{},\"align\":{},\"reason\":",
            candidate.size, candidate.align
        )?;
        write_json_string(&mut out, candidate.reason)?;
        write!(out, ",\"fields\":[")?;
        for (field_idx, field) in candidate.fields.iter().enumerate() {
            if field_idx > 0 {
                write!(out, ",")?;
            }
            write!(out, "{{\"rust\":")?;
            write_json_string(&mut out, field.rust)?;
            write!(out, ",\"musl\":")?;
            write_json_string(&mut out, field.musl)?;
            write!(out, ",\"offset\":{}}}", field.offset)?;
        }
        write!(out, "]}}")?;
    }
    writeln!(out, "\n]}}")
}
