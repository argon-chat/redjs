//! Canvas effects are evaluated in bitmap space, after globalAlpha and before clipping.

#[derive(Clone, Debug, Default)]
pub struct Effects {
    pub blurs: Vec<f32>,
    pub shadow: Option<Shadow>,
}

#[derive(Clone, Debug)]
pub struct Shadow {
    pub color: [f32; 4],
    pub x: f32,
    pub y: f32,
    /// Canvas shadowBlur is twice the Gaussian standard deviation.
    pub sigma: f32,
}

impl Effects {
    pub fn is_empty(&self) -> bool { self.blurs.is_empty() && self.shadow.is_none() }
}

/// Recognize the implemented CSS blur list. Unsupported valid filters still
/// produce an explicit advisory at the JS boundary, never a renderer panic.
pub fn blurs(value: &str) -> Option<Vec<f32>> {
    let mut rest = value.trim();
    if rest == "none" { return Some(Vec::new()); }
    let mut result = Vec::new();
    while !rest.is_empty() {
        rest = rest.strip_prefix("blur(")?;
        let close = rest.find(')')?;
        let arg = rest[..close].trim();
        let n = if arg.is_empty() { 0.0 } else {
            let pixels = arg.strip_suffix("px").unwrap_or(arg);
            let n: f32 = pixels.parse().ok()?;
            if !arg.ends_with("px") && n != 0.0 { return None; }
            n
        };
        if !n.is_finite() || n < 0.0 { return None; }
        if n > 0.0 { result.push(n); }
        rest = rest[close + 1..].trim_start();
    }
    (!value.trim().is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn blur_list() {
        assert_eq!(super::blurs("blur(2px) blur(0) blur(.5px)"), Some(vec![2.0, 0.5]));
        assert_eq!(super::blurs("none"), Some(vec![]));
        for invalid in ["", "blur(-1px)", "blur(NaNpx)", "blur(2)", "blur(2%)", "blur(2px) junk"] {
            assert!(super::blurs(invalid).is_none(), "{invalid}");
        }
    }
}
