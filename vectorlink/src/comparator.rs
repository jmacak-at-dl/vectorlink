use parallel_hnsw::pq::PartialDistance;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::ops::Deref;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{RwLock, RwLockReadGuard};
use std::{path::Path, sync::Arc};

use parallel_hnsw::{pq, Comparator, Serializable, SerializationError, VectorId};

use crate::vecmath::{
    self, Centroid16, Centroid32, Quantized16Embedding, Quantized32Embedding,
    CENTROID_16_BYTE_LENGTH, CENTROID_32_BYTE_LENGTH, QUANTIZED_16_EMBEDDING_LENGTH,
    QUANTIZED_32_EMBEDDING_LENGTH,
};
use crate::vectors::LoadedVec;
use crate::{
    vecmath::{normalized_cosine_distance, Embedding},
    vectors::{Domain, VectorStore},
};

#[derive(Clone)]
pub struct OpenAIComparator {
    pub domain: Arc<Domain>,
    pub store: Arc<VectorStore>,
}

#[derive(Serialize, Deserialize)]
pub struct ComparatorMeta {
    domain: String,
    size: usize,
}

impl Comparator for OpenAIComparator {
    type T = Embedding;
    type Borrowable<'a> = LoadedVec
        where Self: 'a;
    fn lookup(&self, v: VectorId) -> LoadedVec {
        self.store.get_vec(&self.domain, v.0).unwrap().unwrap()
    }

    fn compare_raw(&self, v1: &Embedding, v2: &Embedding) -> f32 {
        normalized_cosine_distance(v1, v2)
    }
}

impl Serializable for OpenAIComparator {
    type Params = Arc<VectorStore>;
    fn serialize<P: AsRef<Path>>(&self, path: P) -> Result<(), SerializationError> {
        let mut comparator_file: std::fs::File =
            OpenOptions::new().write(true).create(true).open(path)?;
        eprintln!("opened comparator serialize file");
        let domain = self.domain.name();
        // How do we get this value?
        let size = 2_000_000;
        let comparator = ComparatorMeta {
            domain: domain.to_string(),
            size,
        };
        let comparator_meta = serde_json::to_string(&comparator)?;
        eprintln!("serialized comparator");
        comparator_file.write_all(&comparator_meta.into_bytes())?;
        eprintln!("wrote comparator to file");
        Ok(())
    }

    fn deserialize<P: AsRef<Path>>(
        path: P,
        store: Arc<VectorStore>,
    ) -> Result<Self, SerializationError> {
        let mut comparator_file = OpenOptions::new().read(true).open(path)?;
        let mut contents = String::new();
        comparator_file.read_to_string(&mut contents)?;
        let ComparatorMeta { domain, size: _ } = serde_json::from_str(&contents)?;
        let domain = store.get_domain(&domain)?;
        Ok(OpenAIComparator { domain, store })
    }
}

#[derive(Default)]
struct MemoizedPartialDistances32 {
    partial_distances: Vec<f32>,
    size: usize,
}

impl MemoizedPartialDistances32 {
    fn new(vectors: &[Centroid32]) -> Self {
        let size = vectors.len();
        let mut partial_distances: Vec<f32> = vec![0.0; size * size];
        for c in 0..size * size {
            let i = c / size;
            let j = c % size;
            partial_distances[c] = vecmath::euclidean_partial_distance_32(&vectors[i], &vectors[j]);
        }

        Self {
            partial_distances,
            size,
        }
    }

    #[allow(dead_code)]
    fn all_distances(&self) -> &[f32] {
        &self.partial_distances
    }

    fn partial_distance(&self, i: u16, j: u16) -> f32 {
        self.partial_distances[(i * self.size as u16 + j) as usize]
    }
}

#[derive(Default)]
struct MemoizedPartialDistances16 {
    partial_distances: Vec<f32>,
    size: usize,
}

impl MemoizedPartialDistances16 {
    fn new(vectors: &[Centroid16]) -> Self {
        let size = vectors.len();
        let mut partial_distances: Vec<f32> = vec![0.0; size * size];
        for c in 0..size * size {
            let i = c / size;
            let j = c % size;
            partial_distances[c] = vecmath::euclidean_partial_distance_16(&vectors[i], &vectors[j]);
        }

        Self {
            partial_distances,
            size,
        }
    }

    #[allow(dead_code)]
    fn all_distances(&self) -> &[f32] {
        &self.partial_distances
    }

    fn partial_distance(&self, i: u16, j: u16) -> f32 {
        self.partial_distances[(i * self.size as u16 + j) as usize]
    }
}

#[derive(Clone, Default)]
pub struct Centroid32Comparator {
    distances: Arc<RwLock<MemoizedPartialDistances32>>,
    centroids: Arc<RwLock<Vec<Centroid32>>>,
}

impl Comparator for Centroid32Comparator {
    type T = Centroid32;

    type Borrowable<'a> = ReadLockedVec<'a, Centroid32>;

    fn lookup(&self, v: VectorId) -> Self::Borrowable<'_> {
        ReadLockedVec {
            lock: self.centroids.read().unwrap(),
            id: v,
        }
    }

    fn compare_raw(&self, v1: &Self::T, v2: &Self::T) -> f32 {
        vecmath::euclidean_distance_32(v1, v2)
    }
}

impl PartialDistance for Centroid32Comparator {
    fn partial_distance(&self, i: u16, j: u16) -> f32 {
        self.distances.read().unwrap().partial_distance(i, j)
    }
}

impl Serializable for Centroid32Comparator {
    type Params = ();

    fn serialize<P: AsRef<Path>>(&self, path: P) -> Result<(), SerializationError> {
        let centroids = self.centroids.read().unwrap();
        let len = centroids.len();
        let buf: &[u8] = unsafe {
            std::slice::from_raw_parts(
                centroids.as_ptr() as *const u8,
                len * std::mem::size_of::<Centroid32>(),
            )
        };
        std::fs::write(path, buf)?;
        Ok(())
    }

    fn deserialize<P: AsRef<Path>>(
        path: P,
        _params: Self::Params,
    ) -> Result<Self, SerializationError> {
        let size = std::fs::metadata(&path)?.size() as usize;
        assert_eq!(0, size % CENTROID_32_BYTE_LENGTH);
        let number_of_centroids = size / CENTROID_32_BYTE_LENGTH;
        let mut vec = vec![Centroid32::default(); number_of_centroids];
        let mut file = std::fs::File::open(&path)?;
        let buf = unsafe { std::slice::from_raw_parts_mut(vec.as_mut_ptr() as *mut u8, size) };
        file.read_exact(buf)?;

        Ok(Self {
            distances: Arc::new(RwLock::new(MemoizedPartialDistances32::new(&vec))),
            centroids: Arc::new(RwLock::new(vec)),
        })
    }
}

impl parallel_hnsw::pq::VectorStore for Centroid32Comparator {
    type T = <Centroid32Comparator as Comparator>::T;

    fn store(&mut self, i: Box<dyn Iterator<Item = Self::T>>) -> Vec<VectorId> {
        let mut data = self.centroids.write().unwrap();
        let vid = data.len();
        let mut vectors: Vec<VectorId> = Vec::new();
        data.extend(i.enumerate().map(|(i, v)| {
            vectors.push(VectorId(vid + i));
            v
        }));
        let distances = MemoizedPartialDistances32::new(&data);
        let mut dist = self.distances.write().unwrap();
        *dist = distances;
        vectors
    }
}

#[derive(Clone, Default)]
pub struct Centroid16Comparator {
    distances: Arc<MemoizedPartialDistances16>,
    centroids: Arc<Vec<Centroid16>>,
}

impl Comparator for Centroid16Comparator {
    type T = Centroid16;

    type Borrowable<'a> = &'a Centroid16;

    fn lookup(&self, v: VectorId) -> Self::Borrowable<'_> {
        &self.centroids[v.0]
    }

    fn compare_raw(&self, v1: &Self::T, v2: &Self::T) -> f32 {
        vecmath::euclidean_distance_16(v1, v2)
    }
}

impl PartialDistance for Centroid16Comparator {
    fn partial_distance(&self, i: u16, j: u16) -> f32 {
        self.distances.partial_distance(i, j)
    }
}

impl Serializable for Centroid16Comparator {
    type Params = ();

    fn serialize<P: AsRef<Path>>(&self, path: P) -> Result<(), SerializationError> {
        let centroids = &self.centroids;
        let len = self.centroids.len();
        let buf: &[u8] = unsafe {
            std::slice::from_raw_parts(
                centroids.as_ptr() as *const u8,
                len * std::mem::size_of::<Centroid16>(),
            )
        };
        std::fs::write(path, buf)?;
        Ok(())
    }

    fn deserialize<P: AsRef<Path>>(
        path: P,
        _params: Self::Params,
    ) -> Result<Self, SerializationError> {
        let size = std::fs::metadata(&path)?.size() as usize;
        assert_eq!(0, size % CENTROID_16_BYTE_LENGTH);
        let number_of_centroids = size / CENTROID_16_BYTE_LENGTH;
        let mut vec = vec![Centroid16::default(); number_of_centroids];
        let mut file = std::fs::File::open(&path)?;
        let buf = unsafe { std::slice::from_raw_parts_mut(vec.as_mut_ptr() as *mut u8, size) };
        file.read_exact(buf)?;

        Ok(Self {
            distances: Arc::new(MemoizedPartialDistances16::new(&vec)),
            centroids: Arc::new(vec),
        })
    }
}

impl parallel_hnsw::pq::VectorStore for Centroid16Comparator {
    type T = <Centroid16Comparator as Comparator>::T;

    fn store(&mut self, i: Box<dyn Iterator<Item = Self::T>>) -> Vec<VectorId> {
        let mut data = (*self.centroids).clone();
        let vid = self.centroids.len();
        let mut vectors: Vec<VectorId> = Vec::new();
        data.extend(i.enumerate().map(|(i, v)| {
            vectors.push(VectorId(vid + i));
            v
        }));
        let distances = MemoizedPartialDistances16::new(&data);
        self.centroids = data;
        let dist = &mut self.distances;
        *dist = distances.into();
        vectors
    }
}

#[derive(Clone)]
pub struct Quantized32Comparator {
    pub cc: Centroid32Comparator,
    pub data: Arc<RwLock<Vec<Quantized32Embedding>>>,
}

#[derive(Clone)]
pub struct Quantized16Comparator {
    pub cc: Centroid16Comparator,
    pub data: Arc<RwLock<Vec<Quantized16Embedding>>>,
}

pub struct ReadLockedVec<'a, T> {
    lock: RwLockReadGuard<'a, Vec<T>>,
    id: VectorId,
}

impl<'a, T> Deref for ReadLockedVec<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.lock[self.id.0]
    }
}

impl PartialDistance for Quantized32Comparator {
    fn partial_distance(&self, i: u16, j: u16) -> f32 {
        self.cc.partial_distance(i, j)
    }
}

impl PartialDistance for Quantized16Comparator {
    fn partial_distance(&self, i: u16, j: u16) -> f32 {
        self.cc.partial_distance(i, j)
    }
}

impl Comparator for Quantized32Comparator
where
    Quantized32Comparator: PartialDistance,
{
    type T = Quantized32Embedding;

    type Borrowable<'a> = ReadLockedVec<'a, Self::T>;

    fn lookup(&self, v: VectorId) -> Self::Borrowable<'_> {
        ReadLockedVec {
            lock: self.data.read().unwrap(),
            id: v,
        }
    }

    fn compare_raw(&self, v1: &Self::T, v2: &Self::T) -> f32 {
        let mut partial_distances = [0.0_f32; QUANTIZED_32_EMBEDDING_LENGTH];
        for ix in 0..QUANTIZED_32_EMBEDDING_LENGTH {
            let partial_1 = v1[ix];
            let partial_2 = v2[ix];
            let partial_distance = self.cc.partial_distance(partial_1, partial_2);
            partial_distances[ix] = partial_distance;
        }

        vecmath::sum_48(&partial_distances).sqrt()
    }
}

impl Serializable for Quantized32Comparator {
    type Params = ();

    fn serialize<P: AsRef<Path>>(&self, path: P) -> Result<(), SerializationError> {
        let path_buf: PathBuf = path.as_ref().into();
        std::fs::create_dir_all(&path_buf)?;

        let index_path = path_buf.join("index");
        self.cc.serialize(index_path)?;

        let vector_path = path_buf.join("vectors");
        let vec_lock = self.data.read().unwrap();
        let size = vec_lock.len() * std::mem::size_of::<Quantized32Embedding>();
        let buf: &[u8] =
            unsafe { std::slice::from_raw_parts(vec_lock.as_ptr() as *const u8, size) };
        std::fs::write(vector_path, buf)?;
        Ok(())
    }

    fn deserialize<P: AsRef<Path>>(
        path: P,
        _params: Self::Params,
    ) -> Result<Self, SerializationError> {
        let path_buf: PathBuf = path.as_ref().into();
        let index_path = path_buf.join("index");
        let cc = Centroid32Comparator::deserialize(index_path, ())?;

        let vector_path = path_buf.join("vectors");

        let size = std::fs::metadata(&vector_path)?.size() as usize;
        assert_eq!(0, size % std::mem::size_of::<Quantized32Embedding>());
        let number_of_quantized = size / std::mem::size_of::<Quantized32Embedding>();
        let mut vec = vec![[0_u16; QUANTIZED_32_EMBEDDING_LENGTH]; number_of_quantized];
        let mut file = std::fs::File::open(&vector_path)?;
        let buf = unsafe { std::slice::from_raw_parts_mut(vec.as_mut_ptr() as *mut u8, size) };
        file.read_exact(buf)?;
        let data = Arc::new(RwLock::new(vec));
        Ok(Self { cc, data })
    }
}

impl pq::VectorStore for Quantized32Comparator {
    type T = <Quantized32Comparator as Comparator>::T;

    fn store(&mut self, i: Box<dyn Iterator<Item = Self::T>>) -> Vec<VectorId> {
        let mut data = self.data.write().unwrap();
        let vid = data.len();
        let mut vectors: Vec<VectorId> = Vec::new();
        data.extend(i.enumerate().map(|(i, v)| {
            vectors.push(VectorId(vid + i));
            v
        }));
        vectors
    }
}

impl pq::VectorSelector for OpenAIComparator {
    type T = Embedding;

    fn selection(&self, size: usize) -> Vec<Self::T> {
        self.store.get_random_vectors(&self.domain, size).unwrap()
    }

    fn vector_chunks(&self) -> impl Iterator<Item = Vec<Self::T>> {
        // low quality make better
        let iter = (0..self.domain.num_vecs())
            .map(|index| *self.store.get_vec(&self.domain, index).unwrap().unwrap());

        ChunkedVecIterator {
            iter,
            _x: PhantomData,
        }
    }
}

impl Comparator for Quantized16Comparator
where
    Quantized16Comparator: PartialDistance,
{
    type T = Quantized16Embedding;

    type Borrowable<'a> = ReadLockedVec<'a, Self::T>;

    fn lookup(&self, v: VectorId) -> Self::Borrowable<'_> {
        ReadLockedVec {
            lock: self.data.read().unwrap(),
            id: v,
        }
    }

    fn compare_raw(&self, v1: &Self::T, v2: &Self::T) -> f32 {
        let mut partial_distances = [0.0_f32; QUANTIZED_16_EMBEDDING_LENGTH];
        for ix in 0..QUANTIZED_16_EMBEDDING_LENGTH {
            let partial_1 = v1[ix];
            let partial_2 = v2[ix];
            let partial_distance = self.cc.partial_distance(partial_1, partial_2);
            partial_distances[ix] = partial_distance;
        }

        vecmath::sum_96(&partial_distances).sqrt()
    }
}

impl Serializable for Quantized16Comparator {
    type Params = ();

    fn serialize<P: AsRef<Path>>(&self, path: P) -> Result<(), SerializationError> {
        let path_buf: PathBuf = path.as_ref().into();
        std::fs::create_dir_all(&path_buf)?;

        let index_path = path_buf.join("index");
        self.cc.serialize(index_path)?;

        let vector_path = path_buf.join("vectors");
        let vec_lock = self.data.read().unwrap();
        let size = vec_lock.len() * std::mem::size_of::<Quantized16Embedding>();
        let buf: &[u8] =
            unsafe { std::slice::from_raw_parts(vec_lock.as_ptr() as *const u8, size) };
        std::fs::write(vector_path, buf)?;
        Ok(())
    }

    fn deserialize<P: AsRef<Path>>(
        path: P,
        _params: Self::Params,
    ) -> Result<Self, SerializationError> {
        let path_buf: PathBuf = path.as_ref().into();
        let index_path = path_buf.join("index");
        let cc = Centroid16Comparator::deserialize(index_path, ())?;

        let vector_path = path_buf.join("vectors");

        let size = std::fs::metadata(&vector_path)?.size() as usize;
        assert_eq!(0, size % std::mem::size_of::<Quantized16Embedding>());
        let number_of_quantized = size / std::mem::size_of::<Quantized16Embedding>();
        let mut vec = vec![[0_u16; QUANTIZED_16_EMBEDDING_LENGTH]; number_of_quantized];
        let mut file = std::fs::File::open(&vector_path)?;
        let buf = unsafe { std::slice::from_raw_parts_mut(vec.as_mut_ptr() as *mut u8, size) };
        file.read_exact(buf)?;
        let data = Arc::new(RwLock::new(vec));
        Ok(Self { cc, data })
    }
}

impl pq::VectorStore for Quantized16Comparator {
    type T = <Quantized16Comparator as Comparator>::T;

    fn store(&mut self, i: Box<dyn Iterator<Item = Self::T>>) -> Vec<VectorId> {
        let mut data = self.data.write().unwrap();
        let vid = data.len();
        let mut vectors: Vec<VectorId> = Vec::new();
        data.extend(i.enumerate().map(|(i, v)| {
            vectors.push(VectorId(vid + i));
            v
        }));
        vectors
    }
}

pub struct ChunkedVecIterator<T, I: Iterator<Item = T>> {
    iter: I,
    _x: PhantomData<T>,
}

impl<T, I: Iterator<Item = T>> Iterator for ChunkedVecIterator<T, I> {
    type Item = Vec<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut chunk = Vec::with_capacity(1_000_000);

        for item in self.iter.by_ref() {
            chunk.push(item);
            if chunk.len() == 16_384 {
                break;
            }
        }

        if chunk.is_empty() {
            None
        } else {
            Some(chunk)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use parallel_hnsw::AbstractVector;

    use crate::comparator::Centroid32Comparator;
    use crate::comparator::Comparator;
    use crate::comparator::MemoizedPartialDistances32;
    #[test]
    fn centroid32test() {
        /*
        let vectors = (0..1000)
            .map(|_| {
                let range = Uniform::from(0.0..1.0);
                let v: Vec<f32> = prng.sample_iter(&range).take(CENTROID_32_LENGTH).collect();
                v
            })
            .collect();
         */
        let vectors = Vec::new();
        let distances = Arc::new(RwLock::new(MemoizedPartialDistances32::new(&vectors)));
        let centroids = Arc::new(RwLock::new(vectors));
        let cc = Centroid32Comparator {
            distances,
            centroids,
        };
        let mut v1 = [0.0_f32; 32];
        v1[0] = 1.0;
        v1[1] = 1.0;
        let mut v2 = [0.0_f32; 32];
        v2[30] = 1.0;
        v2[31] = 1.0;
        let res = cc.compare_vec(AbstractVector::Unstored(&v1), AbstractVector::Unstored(&v2));
        assert_eq!(res, 2.0);
    }
}
