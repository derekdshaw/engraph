use super::util::{combine, tail_lines, truncate_lines};
use super::{FilterCtx, FilterOutput};

pub fn get(ctx: &FilterCtx<'_>) -> FilterOutput {
    FilterOutput {
        text: truncate_lines(ctx.stdout, 100, "resources"),
        filter_id: "kubectl_get",
    }
}

pub fn logs(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    FilterOutput {
        text: tail_lines(&text, 200),
        filter_id: "kubectl_logs",
    }
}

/// `kubectl describe` — keep key sections; drop verbose Spec / Annotations.
/// Section-keeper pattern: track whether the current section is one we want.
pub fn describe(ctx: &FilterCtx<'_>) -> FilterOutput {
    let text = combine(ctx.stdout, ctx.stderr);
    // Section headers in `kubectl describe` output start at column 0 with a
    // capitalized word ending in a colon (e.g. "Name:", "Events:").
    let keep_sections: &[&str] = &[
        "Name:",
        "Namespace:",
        "Status:",
        "Conditions:",
        "Events:",
        "Containers:",
        "Image:",
        "Ports:",
        "Restart Count:",
        "Ready:",
    ];
    let drop_sections: &[&str] = &[
        "Spec:",
        "Annotations:",
        "Labels:",
        "Selector:",
        "Volumes:",
        "Node-Selectors:",
        "Tolerations:",
        "QoS Class:",
    ];
    let mut out = String::with_capacity(text.len() / 2);
    let mut in_dropped_section = false;
    for line in text.lines() {
        let is_header = !line.is_empty() && !line.starts_with(' ') && line.contains(':');
        if is_header {
            let header = line.split_whitespace().next().unwrap_or("");
            if drop_sections.iter().any(|h| line.starts_with(h)) {
                in_dropped_section = true;
                continue;
            }
            if keep_sections.iter().any(|h| line.starts_with(h)) || header.ends_with(':') {
                in_dropped_section = false;
                out.push_str(line);
                out.push('\n');
                continue;
            }
        }
        if !in_dropped_section {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "[engraph: kubectl describe trimmed, exit {}]\n",
        ctx.exit_code
    ));
    FilterOutput {
        text: out,
        filter_id: "kubectl_describe",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_drops_spec_and_annotations() {
        let stdout = "\
Name:         my-pod
Namespace:    default
Status:       Running
Spec:
  Containers:
    foo:
      Image: nginx:latest
Annotations:
  some/key: very-long-value-x100
Events:
  Normal Pulled 5s kubelet Container image already present
";
        let out = describe(&FilterCtx {
            cmd: "kubectl",
            args: &["describe".to_string()],
            stdout,
            stderr: "",
            exit_code: 0,
        });
        assert!(out.text.contains("Name:"));
        assert!(out.text.contains("Status:"));
        assert!(out.text.contains("Events:"));
        assert!(!out.text.contains("very-long-value-x100"));
    }
}
