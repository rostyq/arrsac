#![no_std]

extern crate alloc;
use core::cmp::Reverse;

use alloc::{vec, vec::Vec};
use rand_core::RngCore;
use sample_consensus::{Consensus, Estimator, Model};

/// The ARRSAC algorithm for sample consensus.
///
/// Don't forget to shuffle your input data points to avoid bias before
/// using this consensus process. It will not shuffle your data for you.
/// If you do not shuffle, the output will be biased towards data at the beginning
/// of the inputs.
pub struct Arrsac<R> {
    max_candidate_hypotheses: usize,
    block_size: usize,
    likelihood_ratio_threshold: f32,
    initial_epsilon: f32,
    initial_delta: f32,
    inlier_threshold: f64,
    rng: R,
    random_samples: Vec<u32>,
}

impl<R> Arrsac<R>
where
    R: RngCore,
{
    /// `rng` should have the same properties you would want for a Monte Carlo simulation.
    /// It should generate random numbers quickly without having any discernable patterns.
    ///
    /// The `inlier_threshold` is the one parameter that is always specific to your dataset.
    /// This must be set to the threshold in which a data point's residual is considered an inlier.
    /// Some of the other parameters may need to be configured based on the amount of data,
    /// such as `block_size`, `likelihood_ratio_threshold`, and `block_size`. However,
    /// `inlier_threshold` has to be set based on the residual function used with the model.
    ///
    /// `initial_epsilon` must be higher than `initial_delta`. If you modify these values,
    /// you need to make sure that within one `block_size` the `likelihood_ratio_threshold`
    /// can be reached and a model can be rejected. Basically, make sure that
    /// `((1.0 - delta) / (1.0 - epsilon))^block_size >>> likelihood_ratio_threshold`.
    /// This must be done to ensure outlier models are rejected during the initial generation
    /// phase, which only processes `block_size` datapoints.
    ///
    /// `initial_epsilon` should also be as large as you can set it where it is still relatively
    /// pessimistic. This is so that we can more easily reject a model early in the process
    /// to compute an updated value for delta during the adaptive process. This may not be possible
    /// and will depend on your data.
    pub fn new(inlier_threshold: f64, rng: R) -> Self {
        Self {
            max_candidate_hypotheses: 50,
            block_size: 100,
            likelihood_ratio_threshold: 1e1,
            initial_epsilon: 0.05,
            initial_delta: 0.01,
            inlier_threshold,
            rng,
            random_samples: vec![],
        }
    }

    /// Number of hypotheses that will be generated for each block of data evaluated
    ///
    /// Default: `50`
    pub fn max_candidate_hypotheses(self, max_candidate_hypotheses: usize) -> Self {
        Self {
            max_candidate_hypotheses,
            ..self
        }
    }

    /// Number of data points evaluated before more hypotheses are generated
    ///
    /// Default: `100`
    pub fn block_size(self, block_size: usize) -> Self {
        Self { block_size, ..self }
    }

    /// Once a model reaches this level of unlikelihood, it is rejected. Set this
    /// higher to make it less restrictive, usually at the cost of more execution time.
    ///
    /// Increasing this will make it more likely to find a good result (unless it is set very high).
    ///
    /// Decreasing this will speed up execution.
    ///
    /// This ratio is not exposed as a parameter in the original paper, but is instead computed
    /// recursively for a few iterations. It is roughly equivalent to the **reciprocal** of the
    /// **probability of rejecting a good model**. You can use that to control the probability
    /// that a good model is rejected.
    ///
    /// Default: `1e6`
    pub fn likelihood_ratio_threshold(self, likelihood_ratio_threshold: f32) -> Self {
        Self {
            likelihood_ratio_threshold,
            ..self
        }
    }

    /// Initial anticipated probability of an inlier being part of a good model
    ///
    /// This is an estimation that will be updated as ARRSAC executes. The initial
    /// estimate is purposefully low, which will accept more models. As models are
    /// accepted, it will gradually increase it to match the best model found so far,
    /// which makes it more restrictive.
    ///
    /// Default: `0.5`
    pub fn initial_epsilon(self, initial_epsilon: f32) -> Self {
        Self {
            initial_epsilon,
            ..self
        }
    }

    /// Initial anticipated probability of an inlier being part of a bad model
    ///
    /// This is an estimation that will be updated as ARRSAC executes. The initial
    /// estimate is almost certainly incorrect. This can be modified for different data
    /// to get better/faster results. As models are rejected, it will update this value
    /// until it has evaluated it `max_delta_estimations` times.
    ///
    /// Default: `0.25`
    pub fn initial_delta(self, initial_delta: f32) -> Self {
        Self {
            initial_delta,
            ..self
        }
    }

    /// Residual threshold for determining if a data point is an inlier or an outlier of a model
    pub fn inlier_threshold(self, inlier_threshold: f64) -> Self {
        Self {
            inlier_threshold,
            ..self
        }
    }

    /// Algorithm 3 from "A Comparative Analysis of RANSAC Techniques Leading to Adaptive Real-Time Random Sample Consensus"
    ///
    /// At least at present, this does not use the PROSAC method and instead does completely random sampling.
    ///
    /// Returns the initial models (and their num inliers), `epsilon`, and `delta` in that order.
    fn initial_hypotheses<E, Data>(
        &mut self,
        estimator: &E,
        data: impl Iterator<Item = Data> + Clone,
    ) -> (Vec<(E::Model, usize)>, f32, f32)
    where
        E: Estimator<Data>,
    {
        let mut hypotheses = vec![];
        // We don't want more than `block_size` data points to be used to evaluate models initially.
        let initial_datapoints = core::cmp::min(self.block_size, data.clone().count());
        // Set the best inliers to be the floor of what the number of inliers would need to be to be the initial epsilon.
        let mut best_inliers = (self.initial_epsilon * initial_datapoints as f32) as usize;
        // Set the initial epsilon (inlier ratio in good model).
        let mut epsilon = self.initial_epsilon;
        // Set the initial delta (outlier ratio in good model).
        let mut delta = self.initial_delta;
        let mut positive_likelihood_ratio = delta / epsilon;
        let mut negative_likelihood_ratio = (1.0 - delta) / (1.0 - epsilon);
        let mut current_delta_estimations = 0;
        let mut total_delta_inliers = 0;
        let mut best_inlier_indices = vec![];
        let mut random_hypotheses = vec![];
        // Lets us know if we found a candidate hypothesis that actually has enough inliers for us to generate a model from.
        let mut found_usable_hypothesis = false;
        // Iterate through all the randomly generated hypotheses to update epsilon and delta while finding good models.
        for _ in 0..self.max_candidate_hypotheses {
            if found_usable_hypothesis {
                // If we have found a hypothesis that has a sufficient number of inliers, we randomly sample from its inliers
                // to generate new hypotheses since that is much more likely to generate good ones.
                random_hypotheses.extend(self.generate_random_hypotheses_subset(
                    estimator,
                    data.clone(),
                    &best_inlier_indices,
                ));
            } else {
                // Generate the random hypotheses using all the data, not just the evaluation data.
                random_hypotheses.extend(self.generate_random_hypotheses(estimator, data.clone()));
            }
            for model in random_hypotheses.drain(..) {
                // Check if the model satisfies the ASPRT test on only `inital_datapoints` evaluation data.
                if let Some(inliers) = self.asprt(
                    data.clone().take(initial_datapoints),
                    &model,
                    positive_likelihood_ratio,
                    negative_likelihood_ratio,
                    E::MIN_SAMPLES,
                ) {
                    // If this has the largest support (most inliers) then we update the
                    // approximation of epsilon.
                    if inliers > best_inliers {
                        best_inliers = inliers;
                        // Update epsilon (this can only increase, since there are more inliers).
                        epsilon = inliers as f32 / initial_datapoints as f32;
                        // We need to ensure that the delta is sufficiently lower than epsilon to reach
                        // the likelihood ratio threshold within `block_size` samples.
                        if delta > epsilon * 0.75 {
                            delta = epsilon * 0.75;
                        }
                        // Will decrease positive likelihood ratio.
                        positive_likelihood_ratio = delta / epsilon;
                        // Will increase negative likelihood ratio.
                        negative_likelihood_ratio = (1.0 - delta) / (1.0 - epsilon);

                        // Update the inlier indices appropriately.
                        best_inlier_indices = self.inliers(data.clone(), &model);
                        // Mark that a usable hypothesis has been found.
                        found_usable_hypothesis = true;
                    }
                    hypotheses.push((model, inliers));
                } else {
                    // We want to add the information about inliers of a rejected model to our estimation of delta.
                    total_delta_inliers +=
                        self.count_inliers(data.clone().take(initial_datapoints), &model);
                    current_delta_estimations += 1;
                    // Update delta.
                    delta = total_delta_inliers as f32
                        / (current_delta_estimations * initial_datapoints) as f32;
                    // We need to ensure that the delta is sufficiently lower than epsilon to reach
                    // the likelihood ratio threshold within `block_size` samples.
                    if delta > epsilon {
                        epsilon = delta * 1.25;
                        if epsilon > 1.0 {
                            epsilon = 1.0;
                        }
                    }
                    // May change positive likelihood ratio.
                    positive_likelihood_ratio = delta / epsilon;
                    // May change negative likelihood ratio.
                    negative_likelihood_ratio = (1.0 - delta) / (1.0 - epsilon);
                }
            }
        }

        (hypotheses, epsilon, delta)
    }

    /// Populates `self.random_samples` using a len.
    fn populate_samples(&mut self, num: usize, len: usize) {
        // We can generate no hypotheses if the amout of data is too low.
        if len < num {
            panic!("cannot use arrsac without having enough samples");
        }
        let len = len as u32;
        // Threshold generation below adapted from randomize::RandRangeU32.
        let threshold = len.wrapping_neg() % len;
        self.random_samples.clear();
        for _ in 0..num {
            loop {
                let mul = u64::from(self.rng.next_u32()).wrapping_mul(u64::from(len));
                if mul as u32 >= threshold {
                    let s = (mul >> 32) as u32;
                    if !self.random_samples.contains(&s) {
                        self.random_samples.push(s);
                        break;
                    }
                }
            }
        }
    }

    /// Generates as many hypotheses as one call to `Estimator::estimate()` returns from all data.
    fn generate_random_hypotheses<E, Data>(
        &mut self,
        estimator: &E,
        data: impl Iterator<Item = Data> + Clone,
    ) -> E::ModelIter
    where
        E: Estimator<Data>,
    {
        self.populate_samples(E::MIN_SAMPLES, data.clone().count());
        estimator.estimate(
            self.random_samples
                .iter()
                .map(|&ix| data.clone().nth(ix as usize).unwrap()),
        )
    }

    /// Generates as many hypotheses as one call to `Estimator::estimate()` returns from a subset of the data.
    fn generate_random_hypotheses_subset<E, Data>(
        &mut self,
        estimator: &E,
        data: impl Iterator<Item = Data> + Clone,
        subset: &[usize],
    ) -> E::ModelIter
    where
        E: Estimator<Data>,
    {
        self.populate_samples(E::MIN_SAMPLES, subset.len());
        estimator.estimate(
            core::mem::take(&mut self.random_samples)
                .iter()
                .map(|&ix| data.clone().nth(subset[ix as usize]).unwrap()),
        )
    }

    /// Algorithm 1 in "Randomized RANSAC with Sequential Probability Ratio Test".
    ///
    /// This tests if a model is accepted. Returns `Some(inliers)` if accepted or `None` if rejected.
    ///
    /// `inlier_threshold` - The model residual error threshold between inliers and outliers
    /// `positive_likelihood_ratio` - `δ / ε`
    /// `negative_likelihood_ratio` - `(1 - δ) / (1 - ε)`
    fn asprt<Data, M: Model<Data>>(
        &self,
        data: impl Iterator<Item = Data>,
        model: &M,
        positive_likelihood_ratio: f32,
        negative_likelihood_ratio: f32,
        minimum_samples: usize,
    ) -> Option<usize> {
        let mut likelihood_ratio = 1.0;
        let mut inliers = 0;
        for data in data {
            likelihood_ratio *= if model.residual(&data) < self.inlier_threshold {
                inliers += 1;
                positive_likelihood_ratio
            } else {
                negative_likelihood_ratio
            };

            if likelihood_ratio > self.likelihood_ratio_threshold || likelihood_ratio.is_nan() {
                return None;
            }
        }

        (inliers >= minimum_samples).then(|| inliers)
    }

    /// Determines the number of inliers a model has.
    fn count_inliers<Data, M: Model<Data>>(
        &self,
        data: impl Iterator<Item = Data>,
        model: &M,
    ) -> usize {
        data.filter(|data| model.residual(data) < self.inlier_threshold)
            .count()
    }

    /// Gets indices of inliers for a model.
    fn inliers<Data, M: Model<Data>>(
        &self,
        data: impl Iterator<Item = Data>,
        model: &M,
    ) -> Vec<usize> {
        data.enumerate()
            .filter(|(_, data)| model.residual(data) < self.inlier_threshold)
            .map(|(ix, _)| ix)
            .collect()
    }
}

impl<E, R, Data> Consensus<E, Data> for Arrsac<R>
where
    E: Estimator<Data>,
    R: RngCore,
{
    type Inliers = Vec<usize>;

    fn model<I>(&mut self, estimator: &E, data: I) -> Option<E::Model>
    where
        I: Iterator<Item = Data> + Clone,
    {
        self.model_inliers(estimator, data).map(|(model, _)| model)
    }

    fn model_inliers<I>(&mut self, estimator: &E, data: I) -> Option<(E::Model, Self::Inliers)>
    where
        I: Iterator<Item = Data> + Clone,
    {
        // Don't do anything if we don't have enough data.
        if data.clone().count() < E::MIN_SAMPLES {
            return None;
        }
        // Generate the initial set of hypotheses. This also gets us an estimate of epsilon and delta.
        // We only want to give it one block size of data for the initial generation.
        let (mut hypotheses, _, mut delta) = self.initial_hypotheses(estimator, data.clone());

        let mut random_hypotheses = Vec::new();

        // Retain the hypotheses the initial time. This is done before the loop to ensure that if the
        // number of datapoints is too low and the for loop never executes that the best model is returned.
        hypotheses.sort_unstable_by_key(|&(_, inliers)| Reverse(inliers));
        hypotheses.truncate(self.max_candidate_hypotheses);

        // If there are no initial hypotheses or the best hypothesis doesnt have enough inliers then don't bother doing anything.
        if hypotheses.is_empty()
            || self.inliers(data.clone(), &hypotheses[0].0).len() <= E::MIN_SAMPLES
        {
            return None;
        }

        // Gradually increase how many datapoints we are evaluating until we evaluate them all.
        'outer: for block in 1.. {
            let samples_up_to_beginning_of_block = block * self.block_size;
            let samples_up_to_end_of_block = samples_up_to_beginning_of_block + self.block_size;
            // Score hypotheses with samples.
            for sample in samples_up_to_beginning_of_block..samples_up_to_end_of_block {
                // Score the hypotheses with the new datapoint.
                let new_datapoint = if let Some(datapoint) = data.clone().nth(sample) {
                    datapoint
                } else {
                    // We reached the last datapoint, so break out of the outer loop.
                    break 'outer;
                };
                for (hypothesis, inlier_count) in hypotheses.iter_mut() {
                    if hypothesis.residual(&new_datapoint) < self.inlier_threshold {
                        *inlier_count += 1;
                    }
                }
            }
            // First, update epsilon using the best model.
            // Technically model 0 might no longer be the best model after evaluating the last data-point,
            // but that is not that important.
            let epsilon = hypotheses[0].1 as f32 / samples_up_to_end_of_block as f32;
            // We need to ensure that the delta is sufficiently lower than epsilon to reach
            // the likelihood ratio threshold within `block_size` samples.
            if delta > epsilon * 0.75 {
                delta = epsilon * 0.75;
            }
            // Create the likelihood ratios for inliers and outliers.
            let positive_likelihood_ratio = delta / epsilon;
            let negative_likelihood_ratio = (1.0 - delta) / (1.0 - epsilon);
            // Generate the list of inliers for the best model.
            let inliers = self.inliers(data.clone(), &hypotheses[0].0);
            // We generate hypotheses until we reach the initial num hypotheses.
            // We can't count the number generated because it could generate 0 hypotheses
            // and then the loop would continue indefinitely.
            for _ in 0..self.max_candidate_hypotheses {
                random_hypotheses.extend(self.generate_random_hypotheses_subset(
                    estimator,
                    data.clone(),
                    &inliers,
                ));
                for model in random_hypotheses.drain(..) {
                    if let Some(inliers) = self.asprt(
                        data.clone().take(samples_up_to_end_of_block),
                        &model,
                        positive_likelihood_ratio,
                        negative_likelihood_ratio,
                        E::MIN_SAMPLES,
                    ) {
                        hypotheses.push((model, inliers));
                    }
                }
            }
            // This will retain at least half of the hypotheses each time
            // and gradually decrease as the number of samples we are evaluating increases.
            // NOTE: The paper says to run this outside of this if statement, but that
            // seems incorrect intuitively, other wise the minimum would cause the
            // number of models to halve on every single data point.
            // At least halving on every block makes more sense.
            // The paper also says to use a peculiar formula that just results in doing
            // this basic right shift below.
            hypotheses.sort_unstable_by_key(|&(_, inliers)| Reverse(inliers));
            hypotheses.truncate(self.max_candidate_hypotheses >> block);
            if hypotheses.len() <= 1 {
                break 'outer;
            }
        }
        hypotheses
            .into_iter()
            .max_by_key(|&(_, inliers)| inliers)
            .map(|(model, _)| {
                let inliers = self.inliers(data.clone(), &model);
                (model, inliers)
            })
    }
}
