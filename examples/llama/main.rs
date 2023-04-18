// An implementation of LLaMA https://github.com/facebookresearch/llama
// This only contains the inference part as the xla crate does not support backpropagation.
//
// This is based on nanoGPT in a similar way to:
// https://github.com/Lightning-AI/lit-llama/blob/main/lit_llama/model.py
use anyhow::Result;

extern crate xla;
use xla::{Literal, PrimitiveType, XlaBuilder, XlaOp};

mod var_store;
use var_store::VarStore;

const ET: PrimitiveType = PrimitiveType::F32;
const TEMPERATURE: f32 = 0.8f32;
const USE_CPU: bool = false;

#[allow(dead_code)]
struct Config {
    block_size: usize,
    vocab_size: usize,
    n_layer: usize,
    n_head: usize,
    n_embd: usize,
}

#[allow(dead_code)]
impl Config {
    fn config_7b() -> Self {
        Self { block_size: 4096, vocab_size: 32000, n_layer: 32, n_head: 32, n_embd: 4096 }
    }

    fn config_13b() -> Self {
        Self { block_size: 4096, vocab_size: 32000, n_layer: 40, n_head: 40, n_embd: 5120 }
    }

    fn config_30b() -> Self {
        Self { block_size: 4096, vocab_size: 32000, n_layer: 60, n_head: 52, n_embd: 6656 }
    }

    fn config_65b() -> Self {
        Self { block_size: 4096, vocab_size: 32000, n_layer: 80, n_head: 64, n_embd: 8192 }
    }
}

struct Embedding {
    embeddings: Literal,
}

impl Embedding {
    fn new(mut vs: VarStore, vocab_size: usize, n_embd: usize) -> Result<Self> {
        let embeddings = vs.take("weight", ET, &[vocab_size, n_embd])?;
        Ok(Self { embeddings })
    }

    fn forward(&self, indexes: &XlaOp) -> Result<XlaOp> {
        let embeddings = indexes.builder().constant_literal(&self.embeddings);
        let features = embeddings.take(indexes, 0)?;
        Ok(features)
    }
}

struct Linear {
    ws: Literal,
    bs: Option<Literal>,
    out_size: usize,
}

impl Linear {
    fn new(mut vs: VarStore, in_size: usize, out_size: usize) -> Result<Self> {
        let ws = vs.take("weight", ET, &[in_size, out_size])?;
        let bs = vs.take("bias", ET, &[out_size])?;
        Ok(Self { ws, bs: Some(bs), out_size })
    }

    fn new_no_bias(mut vs: VarStore, in_size: usize, out_size: usize) -> Result<Self> {
        let ws = vs.take("weight", ET, &[in_size, out_size])?;
        Ok(Self { ws, bs: None, out_size })
    }

    fn forward(&self, x: &XlaOp) -> Result<XlaOp> {
        let b = x.builder();
        let x_rank = x.rank()?;
        let ws = b.constant_literal(&self.ws);
        let x = x.dot_general(&ws, &[x_rank as i64 - 1], &[0], &[], &[])?;
        let y = match &self.bs {
            None => x,
            Some(bs) => {
                let bs = b.constant_literal(bs).reshape(&[1, 1, self.out_size as i64])?;
                (x + bs)?
            }
        };
        Ok(y)
    }
}

struct RmsNorm {
    scale: Literal,
    size: i64,
}

impl RmsNorm {
    fn new(mut vs: VarStore, size: usize) -> Result<Self> {
        let scale = vs.take("weight", ET, &[size])?;
        Ok(Self { scale, size: size as i64 })
    }

    fn forward(&self, x: &XlaOp) -> Result<XlaOp> {
        let builder = x.builder();
        let eps = builder.c0(1e-5).convert_element_type(ET)?;
        let norm_x = (x * x)?.reduce_mean(&[-1], true)?;
        let x_normed = (x * (norm_x + eps)?.rsqrt()?)?;
        let scale = builder.constant_literal(&self.scale).reshape(&[1, 1, self.size])?;
        Ok((scale * x_normed)?)
    }
}

struct Mlp {
    c_fc1: Linear,
    c_fc2: Linear,
    c_proj: Linear,
}

impl Mlp {
    fn new(vs: VarStore, n_embd: usize) -> Result<Self> {
        let n_hidden = 8 * n_embd / 3;
        let n_hidden = (n_hidden - 1) / 256 * 256 + 256;
        let c_fc1 = Linear::new_no_bias(&vs / "c_fc1", n_embd, n_hidden)?;
        let c_fc2 = Linear::new_no_bias(&vs / "c_fc2", n_embd, n_hidden)?;
        let c_proj = Linear::new_no_bias(&vs / "c_proj", n_hidden, n_embd)?;
        Ok(Self { c_fc1, c_fc2, c_proj })
    }

    fn forward(&self, x: &XlaOp) -> Result<XlaOp> {
        // TODO: Add silu.
        let x = (self.c_fc1.forward(x)? * self.c_fc2.forward(x)?)?;
        self.c_proj.forward(&x)
    }
}

fn masked_fill<T: xla::NativeType>(on_false: &XlaOp, mask: &XlaOp, on_true: T) -> Result<XlaOp> {
    let shape = mask.shape()?;
    let on_true = mask.builder().c0(on_true).broadcast(shape.dimensions())?;
    let m = mask.select(&on_true, on_false)?;
    Ok(m)
}

struct CausalSelfAttention {
    c_attn: Linear,
    c_proj: Linear,
    n_head: usize,
    n_embd: usize,
}
impl CausalSelfAttention {
    fn new(vs: VarStore, n_head: usize, n_embd: usize) -> Result<Self> {
        let c_attn = Linear::new(&vs / "c_attn", n_embd, 3 * n_embd)?;
        let c_proj = Linear::new(&vs / "c_proj", n_embd, n_embd)?;
        Ok(Self { c_attn, c_proj, n_head, n_embd })
    }

    fn forward(&self, x: &XlaOp) -> Result<XlaOp> {
        let builder = x.builder();
        let (b, t, c) = x.dim3()?;
        let (b, t, c) = (b as i64, t as i64, c as i64);
        let qkv = self.c_attn.forward(x)?;
        let n_embd = self.n_embd as i64;
        let q = qkv.slice_in_dim1(0, n_embd, 2)?;
        let k = qkv.slice_in_dim1(n_embd, 2 * n_embd, 2)?;
        let v = qkv.slice_in_dim1(2 * n_embd, 3 * n_embd, 2)?;
        let target_dim = [b, t, self.n_head as i64, c / self.n_head as i64];
        let k = k.reshape(&target_dim)?.swap_dims(1, 2)?;
        let q = q.reshape(&target_dim)?.swap_dims(1, 2)?;
        let v = v.reshape(&target_dim)?.swap_dims(1, 2)?;
        // TODO: Apply rope to q and k
        //
        let k_shape = k.shape()?;
        let att = (q.matmul(&k.swap_dims(-2, -1)?)?
            * builder.c0(1f32 / (k_shape.last_dim().unwrap() as f32).sqrt()))?;
        let mask = builder
            .one(PrimitiveType::S32)
            .broadcast(&[t, t])?
            .lower_triangle()?
            .reshape(&[1, 1, t, t])?;
        let zero = builder.zero(PrimitiveType::S32).broadcast(&[b, self.n_head as i64, t, t])?;
        let att = masked_fill(&att, &mask.eq(&zero)?, f32::NEG_INFINITY)?;
        let y = att.softmax(-1)?.matmul(&v)?;
        let y = y.swap_dims(1, 2)?.reshape(&[b, t, c])?;
        let y = self.c_proj.forward(&y)?;
        Ok(y)
    }
}

struct Block {
    rms_1: RmsNorm,
    attn: CausalSelfAttention,
    rms_2: RmsNorm,
    mlp: Mlp,
}

impl Block {
    fn new(vs: VarStore, config: &Config) -> Result<Self> {
        let rms_1 = RmsNorm::new(&vs / "rms_1", config.n_embd)?;
        let attn = CausalSelfAttention::new(&vs / "attn", config.n_head, config.n_embd)?;
        let rms_2 = RmsNorm::new(&vs / "rms_2", config.n_embd)?;
        let mlp = Mlp::new(&vs / "mlp", config.n_embd)?;
        Ok(Self { rms_1, attn, rms_2, mlp })
    }

    fn forward(&self, x: &XlaOp) -> Result<XlaOp> {
        let x = (self.attn.forward(&self.rms_1.forward(x)?)? + x)?;
        let x = (self.mlp.forward(&self.rms_2.forward(&x)?)? + x)?;
        Ok(x)
    }
}

struct Llama {
    wte: Embedding,
    blocks: Vec<Block>,
    ln_f: RmsNorm,
    lm_head: Linear,
}

impl Llama {
    fn new(vs: VarStore, config: &Config) -> Result<Self> {
        let lm_head = Linear::new_no_bias(&vs / "lm_head", config.n_embd, config.vocab_size)?;
        let wte = Embedding::new(&vs / "transformer" / "wte", config.vocab_size, config.n_embd)?;
        let blocks = (0..config.n_layer)
            .map(|i| Block::new(&vs / "transformer" / "h" / i, config))
            .collect::<Result<Vec<_>>>()?;
        let ln_f = RmsNorm::new(&vs / "transformer" / "ln_f", config.n_embd)?;
        Ok(Self { wte, blocks, ln_f, lm_head })
    }

    fn forward(&self, x: &XlaOp) -> Result<XlaOp> {
        let t = x.dim2()?.1 as i64;
        let mut x = self.wte.forward(x)?;
        for block in self.blocks.iter() {
            x = block.forward(&x)?;
        }
        let x = self.ln_f.forward(&x)?;
        let x = x.slice_in_dim1(t - 1, t, 1)?;
        let logits = self.lm_head.forward(&x)?;
        Ok(logits)
    }
}

fn llama_computation(vs: VarStore, bsize: i64) -> Result<xla::XlaComputation> {
    let b = XlaBuilder::new("llama");
    let config = Config::config_7b();
    let llama = Llama::new(vs, &config)?;
    let input = b.parameter(0, PrimitiveType::S32, &[bsize, config.block_size as i64], "tokens");
    let logits = llama.forward(&input)?;
    let prs = (logits / b.c0(TEMPERATURE))?.softmax(-1)?;
    Ok(prs.build()?)
}

fn main() -> Result<()> {
    let client = if USE_CPU { xla::PjRtClient::cpu()? } else { xla::PjRtClient::gpu(0.95, false)? };
    println!("{} {} {}", client.platform_name(), client.platform_version(), client.device_count());
    let start_load = std::time::Instant::now();
    let vs = VarStore::new("llama.npz")?;
    println!("loaded {} literals in {:?}", vs.len(), start_load.elapsed());
    let start_build = std::time::Instant::now();
    let llama = llama_computation(vs, 1)?;
    println!("generated the computation in {:?}", start_build.elapsed());
    let start_compile = std::time::Instant::now();
    let _llama_exe = client.compile(&llama)?;
    println!("compiled the executable in {:?}", start_compile.elapsed());
    Ok(())
}