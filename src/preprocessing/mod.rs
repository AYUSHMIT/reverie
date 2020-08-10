mod constants;
pub mod preprocessing;
pub mod prover;
pub mod verifier;

use crate::algebra::*;
use crate::consts::*;
use crate::crypto::*;
use crate::fs::*;
use crate::util::*;
use crate::Instruction;

use std::marker::PhantomData;
use std::mem;
use std::sync::Arc;

use async_channel::{Receiver, SendError, Sender};
use async_std::task;

use serde::{Deserialize, Serialize};

pub struct SharesGenerator<D: Domain> {
    pub input: ShareGenerator<D>,
    pub branch: ShareGenerator<D>,
    pub beaver: ShareGenerator<D>,
}

pub fn branch_permutation(seed: &[u8; KEY_SIZE], branches: usize) -> Vec<usize> {
    random_permutation(
        &mut PRG::new(kdf(CONTEXT_RNG_BRANCH_PERMUTE, seed)),
        branches,
    )
}

impl<D: Domain> SharesGenerator<D> {
    pub fn new(player_seeds: &[[u8; KEY_SIZE]]) -> Self {
        let input_prgs: Vec<PRG> = player_seeds
            .iter()
            .map(|seed| PRG::new(kdf(CONTEXT_RNG_INPUT_MASK, seed)))
            .collect();

        let branch_prgs: Vec<PRG> = player_seeds
            .iter()
            .map(|seed| PRG::new(kdf(CONTEXT_RNG_BRANCH_MASK, seed)))
            .collect();

        let beaver_prgs: Vec<PRG> = player_seeds
            .iter()
            .map(|seed| PRG::new(kdf(CONTEXT_RNG_BEAVER, seed)))
            .collect();

        Self {
            input: ShareGenerator::new(input_prgs),
            branch: ShareGenerator::new(branch_prgs),
            beaver: ShareGenerator::new(beaver_prgs),
        }
    }
}

pub struct ShareGenerator<D: Domain> {
    batches: Vec<D::Batch>,
    shares: Vec<D::Sharing>,
    next: usize,
    prgs: Vec<PRG>,
}

impl<D: Domain> ShareGenerator<D> {
    pub fn new(prgs: Vec<PRG>) -> Self {
        debug_assert_eq!(prgs.len(), D::PLAYERS);
        ShareGenerator {
            batches: vec![D::Batch::ZERO; D::PLAYERS],
            shares: vec![D::Sharing::ZERO; D::Batch::DIMENSION],
            next: D::Batch::DIMENSION,
            prgs,
        }
    }

    pub fn next(&mut self) -> D::Sharing {
        if self.next >= self.shares.len() {
            for i in 0..D::PLAYERS {
                self.batches[i] = D::Batch::gen(&mut self.prgs[i]);
            }
            D::convert(&mut self.shares[..], &self.batches);
            self.next = 0;
        }
        let elem = self.shares[self.next];
        self.next += 1;
        elem
    }

    pub fn batches(&self) -> &[D::Batch] {
        &self.batches[..]
    }

    pub fn is_empty(&self) -> bool {
        return self.next == D::Batch::DIMENSION;
    }

    pub fn empty(&mut self) {
        self.next = D::Batch::DIMENSION;
    }
}

struct Player {
    input: PRG,
    beaver: PRG,
    branch: PRG,
}

impl Player {
    fn new(seed: &[u8; KEY_SIZE]) -> Self {
        Player {
            input: PRG::new(kdf(CONTEXT_RNG_INPUT_MASK, seed)),
            beaver: PRG::new(kdf(CONTEXT_RNG_BEAVER, seed)),
            branch: PRG::new(kdf(CONTEXT_RNG_BRANCH_MASK, seed)),
        }
    }
}

async fn feed<D: Domain, PI: Iterator<Item = Instruction<D::Scalar>>>(
    senders: &mut [Sender<Arc<Vec<Instruction<D::Scalar>>>>],
    program: &mut PI,
) -> bool {
    // next slice of program
    let ps = Arc::new(read_n(program, BATCH_SIZE));
    if ps.len() == 0 {
        return false;
    }

    // feed to workers
    for sender in senders {
        sender.send(ps.clone()).await.unwrap();
    }
    true
}

/// Represents repeated execution of the preprocessing phase.
/// The preprocessing phase is executed D::ONLINE_REPETITIONS times, then fed to a random oracle,
/// which dictates the subset of executions to open.
#[derive(Clone, Serialize, Deserialize)]
pub struct Proof<D: Domain> {
    hidden: Vec<Hash>, // commitments to the hidden pre-processing executions
    random: TreePRF, // punctured PRF used to derive the randomness for the opened pre-processing executions
    _ph: PhantomData<D>,
}

pub struct Run {
    pub(crate) seed: [u8; KEY_SIZE], // root seed
    pub(crate) union: Hash,
    pub(crate) commitments: Vec<Hash>, // preprocessing commitment for every player
}

/// Represents the randomness for the preprocessing executions used during the online execution.
///
/// Reusing a PreprocessingOutput for multiple proofs violates zero-knowledge:
/// leaking the witness / input to the program.
///
/// For this reason PreprocessingOutput does not implement Copy/Clone
/// and the online phase takes ownership of the struct, nor does it expose any fields.
pub struct PreprocessingOutput<D: Domain> {
    pub(crate) branches: Arc<Vec<Vec<D::Batch>>>,
    pub(crate) hidden: Vec<Run>,
}

pub struct Output<D: Domain> {
    pub(crate) hidden: Vec<Hash>,
    _ph: PhantomData<D>,
}

pub fn pack_branch<D: Domain>(branch: &[D::Scalar]) -> Vec<D::Batch> {
    let mut res: Vec<D::Batch> = Vec::with_capacity(branch.len() / D::Batch::DIMENSION + 1);
    for chunk in branch.chunks(D::Batch::DIMENSION) {
        res.push(if chunk.len() < D::Batch::DIMENSION {
            // copy and pad with zero elements
            let mut batch = vec![D::Scalar::ZERO; D::Batch::DIMENSION];
            batch[..chunk.len()].copy_from_slice(chunk);
            <D::Batch as RingModule<D::Scalar>>::pack(&batch[..])
        } else {
            // zero copy
            <D::Batch as RingModule<D::Scalar>>::pack(chunk)
        })
    }
    res
}

pub fn pack_branches<D: Domain>(branches: &[&[D::Scalar]]) -> Vec<Vec<D::Batch>> {
    let mut batches: Vec<Vec<D::Batch>> = Vec::with_capacity(branches.len());
    for branch in branches {
        batches.push(pack_branch::<D>(branch));
    }
    batches
}

impl<D: Domain> Proof<D> {
    async fn preprocess<PI: Iterator<Item = Instruction<D::Scalar>>>(
        seeds: &[[u8; KEY_SIZE]],
        branches: Arc<Vec<Vec<D::Batch>>>,
        mut program: PI,
    ) -> Vec<(Hash, Vec<Hash>)> {
        assert!(
            branches.len() > 0,
            "even when the branch feature is not used, the branch should still be provided and should be a singleton list with an empty element"
        );

        async fn process<D: Domain>(
            root: [u8; KEY_SIZE],
            branches: Arc<Vec<Vec<D::Batch>>>,
            outputs: Sender<()>,
            inputs: Receiver<Arc<Vec<Instruction<D::Scalar>>>>,
        ) -> Result<(Hash, Vec<Hash>), SendError<()>> {
            let mut preprocessing: preprocessing::PreprocessingExecution<D> =
                preprocessing::PreprocessingExecution::new(root, &branches[..]);

            loop {
                match inputs.recv().await {
                    Ok(program) => {
                        preprocessing.prove(&program[..]);
                        outputs.send(()).await?;
                    }
                    Err(_) => {
                        return Ok(preprocessing.done());
                    }
                }
            }
        }

        // create async parallel task for every repetition
        let mut tasks = Vec::with_capacity(D::PREPROCESSING_REPETITIONS);
        let mut inputs: Vec<Sender<Arc<Vec<Instruction<D::Scalar>>>>> =
            Vec::with_capacity(D::PREPROCESSING_REPETITIONS);
        let mut outputs = Vec::with_capacity(D::PREPROCESSING_REPETITIONS);

        for seed in seeds.iter().cloned() {
            let (send_inputs, recv_inputs) = async_channel::bounded(5);
            let (send_outputs, recv_outputs) = async_channel::bounded(5);
            tasks.push(task::spawn(process::<D>(
                seed,
                branches.clone(),
                send_outputs,
                recv_inputs,
            )));
            inputs.push(send_inputs);
            outputs.push(recv_outputs);
        }

        // schedule up to 2 tasks immediately (for better performance)
        let mut scheduled = 0;
        scheduled += feed::<D, _>(&mut inputs[..], &mut program).await as usize;
        scheduled += feed::<D, _>(&mut inputs[..], &mut program).await as usize;

        // wait for all scheduled tasks to complete
        while scheduled > 0 {
            for rx in outputs.iter_mut() {
                let _ = rx.recv().await;
            }
            scheduled -= 1;
            scheduled += feed::<D, _>(&mut inputs[..], &mut program).await as usize;
        }

        // close inputs channels
        inputs.clear();

        // collect final commitments
        let mut results: Vec<(Hash, Vec<Hash>)> = Vec::with_capacity(D::PREPROCESSING_REPETITIONS);
        for t in tasks.into_iter() {
            results.push(t.await.unwrap());
        }
        results
    }

    pub async fn verify<PI: Iterator<Item = Instruction<D::Scalar>>>(
        &self,
        branches: &[&[D::Scalar]],
        program: PI,
    ) -> Option<Output<D>> {
        // pack branch scalars into batches for efficiency
        let branches = Arc::new(pack_branches::<D>(branches));

        // derive keys and hidden execution indexes
        let mut roots: Vec<Option<[u8; KEY_SIZE]>> = vec![None; D::PREPROCESSING_REPETITIONS];
        self.random.expand(&mut roots);

        // derive the hidden indexes
        let mut opened: Vec<bool> = Vec::with_capacity(D::PREPROCESSING_REPETITIONS);
        let mut hidden: Vec<usize> = Vec::with_capacity(D::ONLINE_REPETITIONS);
        for (i, key) in roots.iter().enumerate() {
            opened.push(key.is_some());
            if key.is_none() {
                hidden.push(i)
            }
        }

        // prover must open exactly R-H repetitions
        if hidden.len() != D::ONLINE_REPETITIONS {
            return None;
        }

        // recompute the opened repetitions
        let opened_roots: Vec<[u8; KEY_SIZE]> = roots
            .iter()
            .filter(|v| v.is_some())
            .map(|v| v.unwrap())
            .collect();

        debug_assert_eq!(
            opened_roots.len(),
            D::PREPROCESSING_REPETITIONS - D::ONLINE_REPETITIONS
        );

        let opened_results = Self::preprocess(&opened_roots[..], branches, program).await;

        debug_assert_eq!(
            opened_results.len(),
            D::PREPROCESSING_REPETITIONS - D::ONLINE_REPETITIONS
        );

        // interleave the proved hashes with the recomputed ones
        let mut hashes = Vec::with_capacity(D::PREPROCESSING_REPETITIONS);
        {
            let mut open_hsh = opened_results.iter().map(|(comm, _)| comm);
            let mut hide_hsh = self.hidden.iter();
            for open in opened {
                if open {
                    hashes.push(open_hsh.next().unwrap())
                } else {
                    hashes.push(hide_hsh.next().unwrap())
                }
            }
        }

        debug_assert_eq!(hashes.len(), D::PREPROCESSING_REPETITIONS);

        // feed to the Random-Oracle
        let mut challenge_prg = {
            let mut oracle: View = View::new();
            let mut scope: Scope = oracle.scope(LABEL_SCOPE_AGGREGATE_COMMIT);
            for hash in hashes.iter() {
                scope.join(hash);
            }
            mem::drop(scope);
            oracle.prg(LABEL_RNG_OPEN_PREPROCESSING)
        };

        // accept if the hidden indexes where computed correctly (Fiat-Shamir transform)
        let subset: Vec<usize> = random_subset(
            &mut challenge_prg,
            D::PREPROCESSING_REPETITIONS,
            D::ONLINE_REPETITIONS,
        );
        if &hidden[..] == &subset[..] {
            Some(Output {
                hidden: self.hidden.iter().cloned().collect(),
                _ph: PhantomData,
            })
        } else {
            None
        }
    }

    /// Create a new pre-processing proof.
    ///
    ///
    pub fn new<PI: Iterator<Item = Instruction<D::Scalar>>>(
        global: [u8; KEY_SIZE],
        branches: &[&[D::Scalar]],
        program: PI,
    ) -> (Self, PreprocessingOutput<D>) {
        // pack branch scalars into batches for efficiency
        let branches = Arc::new(pack_branches::<D>(branches));

        // expand the global seed into per-repetition roots
        let mut roots: Vec<[u8; KEY_SIZE]> = vec![[0; KEY_SIZE]; D::PREPROCESSING_REPETITIONS];
        TreePRF::expand_full(&mut roots, global);

        // block and wait for hashes to compute
        let results = task::block_on(Self::preprocess(&roots[..], branches.clone(), program));

        // send the pre-processing commitments to the random oracle, receive challenges
        let mut challenge_prg = {
            let mut oracle: View = View::new();
            let mut scope: Scope = oracle.scope(LABEL_SCOPE_AGGREGATE_COMMIT);
            for (hash, _) in results.iter() {
                scope.update(hash.as_bytes());
            }
            mem::drop(scope);
            oracle.prg(LABEL_RNG_OPEN_PREPROCESSING)
        };

        // interpret the oracle response as a subset of indexes to hide
        // (implicitly: which executions to open)
        let hidden: Vec<usize> = random_subset(
            &mut challenge_prg,
            D::PREPROCESSING_REPETITIONS,
            D::ONLINE_REPETITIONS,
        );

        // puncture the prf at the hidden indexes
        // (implicitly: pass the randomness for all other executions to the verifier)
        let mut tree: TreePRF = TreePRF::new(D::PREPROCESSING_REPETITIONS, global);
        for i in hidden.iter().cloned() {
            tree = tree.puncture(i);
        }

        // extract pre-processing key material for the hidden views
        // (returned to the prover for use in the online phase)
        let mut hidden_runs: Vec<Run> = Vec::with_capacity(D::ONLINE_REPETITIONS);
        let mut hidden_hashes: Vec<Hash> = Vec::with_capacity(D::ONLINE_REPETITIONS);
        let mut results = results.into_iter().enumerate();

        for i in hidden.iter().cloned() {
            // find the matching result
            let result = loop {
                let (j, elem) = results.next().unwrap();
                if i == j {
                    break elem;
                }
            };

            // add to the preprocessing output
            hidden_runs.push(Run {
                seed: roots[i],
                union: result.0.clone(),
                commitments: result.1,
            });

            // add to the preprocessing proof
            hidden_hashes.push(result.0.clone());
        }

        (
            // proof (used by the verifier)
            Proof {
                hidden: hidden_hashes,
                random: tree,
                _ph: PhantomData,
            },
            // randomness used to re-executed the hidden views (used by the prover)
            PreprocessingOutput {
                branches,
                hidden: hidden_runs,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::algebra::gf2::{BitScalar, GF2P8};
    use super::*;

    use rand::Rng;

    #[test]
    fn test_preprocessing_n8() {
        let program = vec![
            Instruction::Input(1),
            Instruction::Input(2),
            Instruction::Mul(0, 1, 2),
        ]; // maybe generate random program?
        let mut rng = rand::thread_rng();
        let seed: [u8; KEY_SIZE] = rng.gen();
        let branch: Vec<BitScalar> = vec![];
        let branches: Vec<&[BitScalar]> = vec![&branch];
        let proof = Proof::<GF2P8>::new(seed, &branches[..], program.iter().cloned());
        assert!(task::block_on(proof.0.verify(&branches[..], program.into_iter())).is_some());
    }
}

#[cfg(test)]
#[cfg(not(debug_assertions))] // omit for testing
mod benchmark {
    use super::super::algebra::gf2::GF2P8;
    use super::*;

    use test::Bencher;

    const MULT: usize = 1_000_000;

    /// Benchmark proof generation of pre-processing using parameters from the paper
    /// (Table 1. p. 10, https://eprint.iacr.org/2018/475/20190311:173838)
    ///
    /// n =   8 (simulated players)
    /// M = 252 (number of pre-processing executions)
    /// t =  44 (online phase executions (hidden pre-processing executions))
    #[bench]
    fn bench_preprocessing_proof_gen_n8(b: &mut Bencher) {
        let mut program = vec![Instruction::Input(1), Instruction::Input(2)];
        program.resize(MULT + 2, Instruction::Mul(0, 1, 2));
        b.iter(|| Proof::<GF2P8>::new([0u8; KEY_SIZE], program.iter().cloned()));
    }

    /*
    /// Benchmark proof verification of pre-processing using parameters from the paper
    /// (Table 1. p. 10, https://eprint.iacr.org/2018/475/20190311:173838)
    ///
    /// n =  64 (simulated players)
    /// M =  23 (number of pre-processing executions)
    /// t =  23 (online phase executions (hidden pre-processing executions))
    #[bench]
    fn bench_preprocessing_proof_verify_n64(b: &mut Bencher) {
        let proof =
            PreprocessedProof::<BitBatch, 64, 64, 631, 1024, 23>::new(BEAVER, [0u8; KEY_SIZE]);
        b.iter(|| proof.verify(BEAVER));
    }
    */

    /// Benchmark proof verification of pre-processing using parameters from the paper
    /// (Table 1. p. 10, https://eprint.iacr.org/2018/475/20190311:173838)
    ///
    /// n =   8 (simulated players)
    /// M = 252 (number of pre-processing executions)
    /// t =  44 (online phase executions (hidden pre-processing executions))
    #[bench]
    fn bench_preprocessing_proof_verify_n8(b: &mut Bencher) {
        let mut program = vec![Instruction::Input(1), Instruction::Input(2)];
        program.resize(MULT + 2, Instruction::Mul(0, 1, 2));
        let (proof, _) = Proof::<GF2P8>::new([0u8; KEY_SIZE], program.iter().cloned());
        b.iter(|| task::block_on(proof.verify(program.iter().cloned())));
    }
}
