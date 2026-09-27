//! Gradient-boosted regression trees, as LightGBM does them at heart: each
//! tree fits what the trees before it got wrong, and splits are found on
//! per-feature histograms (quantile bins), which keeps training fast.
//!
//! Trees suit table data with sharp interactions, like electricity prices:
//! little wind *and* no sun is what makes a price spike.

/// How the trees are grown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub rounds: usize,
    pub learning_rate: f64,
    pub max_depth: usize,
    /// Rows a leaf needs at least.
    pub min_leaf: usize,
    /// L2 regularisation of leaf values.
    pub lambda: f64,
    /// Histogram bins per feature (at most 255).
    pub bins: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            rounds: 300,
            learning_rate: 0.05,
            max_depth: 6,
            min_leaf: 20,
            lambda: 1.0,
            bins: 64,
        }
    }
}

/// A node: a split sends `x[feature] <= threshold` left; a leaf holds a value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Node {
    Split {
        feature: usize,
        threshold: f64,
        left: usize,
        right: usize,
    },
    Leaf(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    fn predict(&self, x: &[f64]) -> f64 {
        let mut i = 0;
        loop {
            match self.nodes[i] {
                Node::Leaf(value) => return value,
                Node::Split {
                    feature,
                    threshold,
                    left,
                    right,
                } => i = if x[feature] <= threshold { left } else { right },
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gbdt {
    pub base: f64,
    pub trees: Vec<Tree>,
}

impl Gbdt {
    pub fn predict(&self, x: &[f64]) -> f64 {
        self.base + self.trees.iter().map(|t| t.predict(x)).sum::<f64>()
    }
}

/// Upper edges of up to `bins` quantile bins of each feature.
fn bin_edges(rows: &[Vec<f64>], features: usize, bins: usize) -> Vec<Vec<f64>> {
    (0..features)
        .map(|f| {
            let mut values: Vec<f64> = rows
                .iter()
                .map(|r| r[f])
                .filter(|v| v.is_finite())
                .collect();
            values.sort_by(f64::total_cmp);
            values.dedup();
            if values.len() <= bins {
                return values;
            }
            (1..=bins)
                .map(|b| values[(b * values.len() / bins).min(values.len()) - 1])
                .collect::<Vec<f64>>()
        })
        .map(|mut edges: Vec<f64>| {
            edges.dedup();
            edges
        })
        .collect()
}

fn bin_of(edges: &[f64], value: f64) -> u8 {
    let bin = edges.partition_point(|&e| e < value);
    bin.min(edges.len().saturating_sub(1)).min(255) as u8
}

/// Fits the trees on `rows` (each `features` long) to `targets`.
pub fn fit(rows: &[Vec<f64>], targets: &[f64], params: &Params) -> Gbdt {
    let n = rows.len();
    if n == 0 {
        return Gbdt {
            base: 0.0,
            trees: Vec::new(),
        };
    }
    let features = rows[0].len();
    let edges = bin_edges(rows, features, params.bins.clamp(2, 255));
    let binned: Vec<Vec<u8>> = rows
        .iter()
        .map(|r| (0..features).map(|f| bin_of(&edges[f], r[f])).collect())
        .collect();
    let base = targets.iter().sum::<f64>() / n as f64;
    let mut prediction = vec![base; n];
    let mut trees = Vec::with_capacity(params.rounds);
    for _ in 0..params.rounds {
        let residual: Vec<f64> = targets
            .iter()
            .zip(&prediction)
            .map(|(t, p)| t - p)
            .collect();
        let mut builder = Builder {
            binned: &binned,
            edges: &edges,
            residual: &residual,
            params,
            nodes: Vec::new(),
        };
        let all: Vec<usize> = (0..n).collect();
        builder.grow(&all, 0);
        let tree = Tree {
            nodes: builder.nodes,
        };
        for (p, row) in prediction.iter_mut().zip(rows) {
            *p += tree.predict(row);
        }
        trees.push(tree);
    }
    Gbdt { base, trees }
}

struct Builder<'a> {
    binned: &'a [Vec<u8>],
    edges: &'a [Vec<f64>],
    residual: &'a [f64],
    params: &'a Params,
    nodes: Vec<Node>,
}

impl Builder<'_> {
    /// Adds the node for `rows` and its subtree; returns its index.
    fn grow(&mut self, rows: &[usize], depth: usize) -> usize {
        let index = self.nodes.len();
        let sum: f64 = rows.iter().map(|&i| self.residual[i]).sum();
        let count = rows.len() as f64;
        let leaf = sum / (count + self.params.lambda) * self.params.learning_rate;
        self.nodes.push(Node::Leaf(leaf));
        if depth >= self.params.max_depth || rows.len() < 2 * self.params.min_leaf {
            return index;
        }
        let Some((feature, bin)) = self.best_split(rows, sum, count) else {
            return index;
        };
        let (left, right): (Vec<usize>, Vec<usize>) =
            rows.iter().partition(|&&i| self.binned[i][feature] <= bin);
        let threshold = self.edges[feature][usize::from(bin)];
        let left = self.grow(&left, depth + 1);
        let right = self.grow(&right, depth + 1);
        self.nodes[index] = Node::Split {
            feature,
            threshold,
            left,
            right,
        };
        index
    }

    /// The split with the largest gain in squared error, if any helps.
    fn best_split(&self, rows: &[usize], sum: f64, count: f64) -> Option<(usize, u8)> {
        let lambda = self.params.lambda;
        let min_leaf = self.params.min_leaf as f64;
        let parent = sum * sum / (count + lambda);
        let mut best: Option<(f64, usize, u8)> = None;
        for (feature, edges) in self.edges.iter().enumerate() {
            let bins = edges.len();
            if bins < 2 {
                continue;
            }
            let mut sums = vec![0.0; bins];
            let mut counts = vec![0.0; bins];
            for &i in rows {
                let b = usize::from(self.binned[i][feature]);
                sums[b] += self.residual[i];
                counts[b] += 1.0;
            }
            let (mut left_sum, mut left_count) = (0.0, 0.0);
            for b in 0..bins - 1 {
                left_sum += sums[b];
                left_count += counts[b];
                let right_count = count - left_count;
                if left_count < min_leaf || right_count < min_leaf {
                    continue;
                }
                let right_sum = sum - left_sum;
                let gain = left_sum * left_sum / (left_count + lambda)
                    + right_sum * right_sum / (right_count + lambda)
                    - parent;
                if gain > best.map_or(1e-9, |(g, _, _)| g) {
                    best = Some((gain, feature, b as u8));
                }
            }
        }
        best.map(|(_, feature, bin)| (feature, bin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_an_interaction() {
        // A "price": high only when both inputs are low (no wind, no sun).
        let rows: Vec<Vec<f64>> = (0..2000)
            .map(|i| vec![f64::from(i % 40) / 40.0, f64::from((i / 40) % 50) / 50.0])
            .collect();
        let target = |r: &[f64]| {
            if r[0] < 0.3 && r[1] < 0.3 {
                100.0
            } else {
                20.0 + 10.0 * r[0]
            }
        };
        let targets: Vec<f64> = rows.iter().map(|r| target(r)).collect();
        let model = fit(&rows, &targets, &Params::default());
        let mae = rows
            .iter()
            .zip(&targets)
            .map(|(r, t)| (model.predict(r) - t).abs())
            .sum::<f64>()
            / rows.len() as f64;
        assert!(mae < 2.0, "{mae}");
        assert!((model.predict(&[0.1, 0.1]) - 100.0).abs() < 5.0);
        assert!((model.predict(&[0.9, 0.9]) - 29.0).abs() < 3.0);
    }
}
