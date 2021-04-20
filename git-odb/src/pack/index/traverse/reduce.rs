use crate::pack::{data::decode, index::traverse};
use git_features::{interrupt::is_triggered, parallel, progress::Progress};
use std::time::Instant;

fn add_decode_result(lhs: &mut decode::Outcome, rhs: decode::Outcome) {
    lhs.num_deltas += rhs.num_deltas;
    lhs.decompressed_size += rhs.decompressed_size;
    lhs.compressed_size += rhs.compressed_size;
    lhs.object_size += rhs.object_size;
}

fn div_decode_result(lhs: &mut decode::Outcome, div: usize) {
    if div != 0 {
        lhs.num_deltas = (lhs.num_deltas as f32 / div as f32) as u32;
        lhs.decompressed_size /= div as u64;
        lhs.compressed_size /= div;
        lhs.object_size /= div as u64;
    }
}

pub struct Reducer<'a, P, E> {
    progress: &'a parking_lot::Mutex<P>,
    check: traverse::SafetyCheck,
    then: Instant,
    entries_seen: usize,
    stats: traverse::Outcome,
    _error: std::marker::PhantomData<E>,
}

impl<'a, P, E> Reducer<'a, P, E>
where
    P: Progress,
{
    pub fn from_progress(
        progress: &'a parking_lot::Mutex<P>,
        pack_data_len_in_bytes: usize,
        check: traverse::SafetyCheck,
    ) -> Self {
        let stats = traverse::Outcome {
            pack_size: pack_data_len_in_bytes as u64,
            ..Default::default()
        };
        Reducer {
            progress: &progress,
            check,
            then: Instant::now(),
            entries_seen: 0,
            stats,
            _error: Default::default(),
        }
    }
}

impl<'a, P, E> parallel::Reduce for Reducer<'a, P, E>
where
    P: Progress,
    E: std::error::Error + Send + Sync + 'static,
{
    type Input = Result<Vec<decode::Outcome>, traverse::Error<E>>;
    type FeedProduce = ();
    type Output = traverse::Outcome;
    type Error = traverse::Error<E>;

    fn feed(&mut self, input: Self::Input) -> Result<(), Self::Error> {
        let chunk_stats: Vec<_> = match input {
            Err(err @ traverse::Error::PackDecode { .. }) if !self.check.fatal_decode_error() => {
                self.progress.lock().info(format!("Ignoring decode error: {}", err));
                return Ok(());
            }
            res => res,
        }?;
        self.entries_seen += chunk_stats.len();

        let chunk_total = chunk_stats.into_iter().fold(
            decode::Outcome::default_from_kind(git_object::Kind::Tree),
            |mut total, stats| {
                *self.stats.objects_per_chain_length.entry(stats.num_deltas).or_insert(0) += 1;
                self.stats.total_decompressed_entries_size += stats.decompressed_size;
                self.stats.total_compressed_entries_size += stats.compressed_size as u64;
                self.stats.total_object_size += stats.object_size as u64;
                use git_object::Kind::*;
                match stats.kind {
                    Commit => self.stats.num_commits += 1,
                    Tree => self.stats.num_trees += 1,
                    Blob => self.stats.num_blobs += 1,
                    Tag => self.stats.num_tags += 1,
                }
                add_decode_result(&mut total, stats);
                total
            },
        );

        add_decode_result(&mut self.stats.average, chunk_total);
        self.progress.lock().set(self.entries_seen);

        if is_triggered() {
            return Err(Self::Error::Interrupted);
        }
        Ok(())
    }

    fn finalize(mut self) -> Result<Self::Output, Self::Error> {
        div_decode_result(&mut self.stats.average, self.entries_seen as usize);

        let elapsed_s = self.then.elapsed().as_secs_f32();
        let objects_per_second = (self.entries_seen as f32 / elapsed_s) as u32;

        self.progress.lock().info(format!(
            "of {} objects done in {:.2}s ({} objects/s, ~{}/s)",
            self.entries_seen,
            elapsed_s,
            objects_per_second,
            bytesize::ByteSize(self.stats.average.object_size * objects_per_second as u64)
        ));
        Ok(self.stats)
    }
}
