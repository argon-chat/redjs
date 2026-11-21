//! A radix-2 complex FFT with precomputed tables, for the convolver and the periodic waves.

pub(in crate::webapi) struct Fft {
    n: usize,
    rev: Vec<u32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Fft {
    /// `n` must be a power of two.
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 2);
        let bits = n.trailing_zeros();
        let rev = (0..n as u32)
            .map(|i| i.reverse_bits() >> (32 - bits))
            .collect();
        let (cos, sin) = (0..n / 2)
            .map(|k| {
                let a = -core::f64::consts::TAU * k as f64 / n as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .unzip();
        Self { n, rev, cos, sin }
    }

    pub fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        debug_assert!(re.len() == n && im.len() == n);
        for i in 0..n {
            let j = self.rev[i] as usize;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut i = 0;
            while i < n {
                for k in 0..half {
                    let (wr, wi) = (self.cos[k * step], self.sin[k * step]);
                    let (a, b) = (i + k, i + k + half);
                    let (vr, vi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                    re[b] = re[a] - vr;
                    im[b] = im[a] - vi;
                    re[a] += vr;
                    im[a] += vi;
                }
                i += len;
            }
            len <<= 1;
        }
    }

    /// The inverse transform, scaled by 1/n.
    pub fn inverse(&self, re: &mut [f32], im: &mut [f32]) {
        self.forward(im, re);
        let k = 1.0 / self.n as f32;
        for v in re.iter_mut().chain(im.iter_mut()) {
            *v *= k;
        }
    }
}
