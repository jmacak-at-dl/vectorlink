use std::{
    any::Any,
    collections::{HashMap, HashSet},
    error::Error,
    io,
    marker::PhantomData,
    ops::{Deref, DerefMut, Range},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use linfa::{traits::Fit, DatasetBase};
use linfa_clustering::KMeans;
use ndarray::{Array, Array2};
use parallel_hnsw::{
    pq::{CentroidComparatorConstructor, HnswQuantizer, Quantizer},
    Comparator, Hnsw, Serializable, VectorId,
};
use rand::{distributions::Uniform, rngs::StdRng, thread_rng, Rng, SeedableRng};
use urlencoding::encode;

use crate::store::{ImmutableVectorFile, LoadedVectorRange, SequentialVectorLoader, VectorFile};

pub trait GenericDomain: 'static + Any + Send + Sync {
    fn name(&self) -> &str;
    fn num_vecs(&self) -> usize;
}

pub fn downcast_generic_domain<T: 'static + Send + Sync>(
    domain: Arc<dyn GenericDomain>,
) -> Arc<Domain<T>> {
    Arc::downcast::<Domain<T>>(domain)
        .expect("Could not downcast domain to expected embedding size")
}

pub trait Deriver: Any {
    type From: Copy;

    fn concatenate_derived(&self, loader: SequentialVectorLoader<Self::From>) -> io::Result<()>;
    fn chunk_size(&self) -> usize {
        1_000
    }
}

pub trait NewDeriver {
    type T: Copy;
    type Deriver: Deriver<From = Self::T>;

    fn new(
        &self,
        path: PathBuf,
        vectors: &VectorFile<Self::T>,
    ) -> Result<Self::Deriver, Box<dyn Error>>;
}

pub struct PqDerivedDomain<
    const SIZE: usize,
    const CENTROID_SIZE: usize,
    const QUANTIZED_SIZE: usize,
    C,
> {
    file: RwLock<VectorFile<[u16; QUANTIZED_SIZE]>>,
    quantizer: HnswQuantizer<SIZE, CENTROID_SIZE, QUANTIZED_SIZE, C>,
}

impl<
        const SIZE: usize,
        const CENTROID_SIZE: usize,
        const QUANTIZED_SIZE: usize,
        C: 'static + Comparator<T = [f32; CENTROID_SIZE]>,
    > Deriver for PqDerivedDomain<SIZE, CENTROID_SIZE, QUANTIZED_SIZE, C>
{
    type From = [f32; SIZE];

    fn concatenate_derived(&self, loader: SequentialVectorLoader<Self::From>) -> io::Result<()> {
        for chunk in loader {
            let chunk = chunk?;
            let mut result = Vec::with_capacity(chunk.len());
            for vec in chunk.iter() {
                let quantized = self.quantizer.quantize(vec);
                result.push(quantized);
            }
            let mut file = self.file.write().unwrap();
            file.append_vector_range(&result)?;
        }

        Ok(())
    }
}

#[derive(Default)]
struct PqDerivedDomainInitializer<
    const SIZE: usize,
    const CENTROID_SIZE: usize,
    const QUANTIZED_SIZE: usize,
    C,
> {
    _x: PhantomData<C>,
}

impl<
        const SIZE: usize,
        const CENTROID_SIZE: usize,
        const QUANTIZED_SIZE: usize,
        C: 'static
            + Comparator<T = [f32; CENTROID_SIZE]>
            + CentroidComparatorConstructor
            + Serializable<Params = ()>,
    > NewDeriver for PqDerivedDomainInitializer<SIZE, CENTROID_SIZE, QUANTIZED_SIZE, C>
{
    type T = [f32; SIZE];
    type Deriver = PqDerivedDomain<SIZE, CENTROID_SIZE, QUANTIZED_SIZE, C>;

    fn new(
        &self,
        path: PathBuf,
        vectors: &VectorFile<[f32; SIZE]>,
    ) -> Result<Self::Deriver, Box<dyn Error>> {
        // TODO do something else for sizes close to number of vecs
        const NUMBER_OF_CENTROIDS: usize = 10_000;
        const SAMPLE_SIZE: usize = NUMBER_OF_CENTROIDS / 10;
        let selection = if SAMPLE_SIZE >= vectors.num_vecs() {
            vectors.all_vectors().unwrap().clone().into_vec()
        } else {
            let mut rng = thread_rng();
            let mut set = HashSet::new();
            let range = Uniform::from(0_usize..vectors.num_vecs());
            while set.len() != SAMPLE_SIZE {
                let candidate = rng.sample(&range);
                set.insert(candidate);
            }

            set.into_iter()
                .map(|index| vectors.vec(index).unwrap())
                .collect()
        };

        // Linfa
        let data: Vec<f32> = selection.into_iter().flat_map(|v| v.into_iter()).collect();
        let sub_length = data.len() / CENTROID_SIZE;
        let sub_arrays = Array::from_shape_vec((sub_length, CENTROID_SIZE), data).unwrap();
        eprintln!("sub_arrays: {sub_arrays:?}");
        let observations = DatasetBase::from(sub_arrays);
        // TODO review this number
        let number_of_clusters = usize::min(sub_length, 1_000);
        let prng = StdRng::seed_from_u64(42);
        eprintln!("Running kmeans");
        let model = KMeans::params_with_rng(number_of_clusters, prng.clone())
            .tolerance(1e-2)
            .fit(&observations)
            .expect("KMeans fitted");
        let centroid_array: Array2<f32> = model.centroids().clone();
        centroid_array.len();
        let centroid_flat: Vec<f32> = centroid_array
            .into_shape(number_of_clusters * CENTROID_SIZE)
            .unwrap()
            .to_vec();
        eprintln!("centroid flat len: {}", centroid_flat.len());
        let centroids: Vec<[f32; CENTROID_SIZE]> = centroid_flat
            .chunks(CENTROID_SIZE)
            .map(|v| {
                let mut array = [0.0; CENTROID_SIZE];
                array.copy_from_slice(v);
                array
            })
            .collect();
        //
        eprintln!("Number of centroids: {}", centroids.len());

        let vector_ids = (0..centroids.len()).map(VectorId).collect();
        let centroid_comparator = C::new(centroids);
        let centroid_m = 24;
        let centroid_m0 = 48;
        let centroid_order = 12;
        let mut centroid_hnsw: Hnsw<C> = Hnsw::generate(
            centroid_comparator,
            vector_ids,
            centroid_m,
            centroid_m0,
            centroid_order,
        );
        //centroid_hnsw.improve_index();
        centroid_hnsw.improve_neighbors(0.01, 1.0);

        let centroid_quantizer: HnswQuantizer<SIZE, CENTROID_SIZE, QUANTIZED_SIZE, C> =
            HnswQuantizer::new(centroid_hnsw);

        let quantizer_path = path.join("quantizer");
        centroid_quantizer.serialize(quantizer_path)?;

        let quantized_path = path.join("quantized.vecs");
        let quantized_file = VectorFile::create(quantized_path, true)?;

        Ok(PqDerivedDomain {
            file: RwLock::new(quantized_file),
            quantizer: centroid_quantizer,
        })
    }
}

pub struct Domain<T> {
    name: String,
    file: RwLock<VectorFile<T>>,
    derived_domains: RwLock<HashMap<String, Arc<dyn Deriver<From = T> + Send + Sync>>>,
}

impl<T: 'static + Copy + Send + Sync> GenericDomain for Domain<T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn num_vecs(&self) -> usize {
        self.file().num_vecs()
    }
}

#[allow(unused)]
impl<T: Copy + 'static> Domain<T> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn num_vecs(&self) -> usize {
        self.file().num_vecs()
    }

    pub fn open<P: AsRef<Path>>(dir: P, name: &str) -> io::Result<Self> {
        let mut path = dir.as_ref().to_path_buf();
        let encoded_name = encode(name);
        path.push(format!("{encoded_name}.vecs"));
        let file = RwLock::new(VectorFile::open_create(&path, true)?);

        Ok(Domain {
            name: name.to_string(),
            derived_domains: RwLock::new(HashMap::new()),
            file,
        })
    }

    pub fn file<'a>(&'a self) -> impl Deref<Target = VectorFile<T>> + 'a {
        self.file.read().unwrap()
    }

    fn file_mut<'a>(&'a self) -> impl DerefMut<Target = VectorFile<T>> + 'a {
        self.file.write().unwrap()
    }

    pub fn immutable_file(&self) -> ImmutableVectorFile<T> {
        self.file().as_immutable()
    }

    pub fn concatenate_file<P: AsRef<Path>>(&self, path: P) -> io::Result<(usize, usize)> {
        let read_vector_file = VectorFile::open(path, true)?;
        let old_size = self.num_vecs();
        let derived_domains = self.derived_domains.read().unwrap();
        for derived in derived_domains.values() {
            let chunk_size = derived.chunk_size();
            derived.concatenate_derived(read_vector_file.vector_chunks(chunk_size)?)?;
        }
        Ok((
            old_size,
            self.file_mut().append_vector_file(&read_vector_file)?,
        ))
    }

    pub fn vec(&self, id: usize) -> io::Result<T> {
        Ok(self.file().vec(id)?)
    }

    pub fn vec_range(&self, range: Range<usize>) -> io::Result<LoadedVectorRange<T>> {
        self.file().vector_range(range)
    }

    pub fn all_vecs(&self) -> io::Result<LoadedVectorRange<T>> {
        self.file().all_vectors()
    }

    pub fn vector_chunks(&self, chunk_size: usize) -> io::Result<SequentialVectorLoader<T>> {
        self.file().vector_chunks(chunk_size)
    }

    pub fn create_derived<
        N: NewDeriver<T = T, Deriver = D>,
        D: Deriver<From = T> + 'static + Send + Sync,
    >(
        &self,
        name: String,
        deriver: N,
    ) -> Result<(), Box<dyn Error>> {
        let mut derived_domains = self.derived_domains.write().unwrap();
        assert!(
            !derived_domains.contains_key(&name),
            "tried to create derived domain that already exists"
        );

        let file = self.file();
        let mut path = file.path().clone();
        path.set_extension("derived");
        path.push(&name);
        std::fs::create_dir_all(&path)?;

        let deriver = deriver.new(path, &*file)?;
        derived_domains.insert(name, Arc::new(deriver));

        Ok(())
    }

    pub fn derived_domain<'a, T2: Deriver + Send + Sync>(
        &'a self,
        name: &str,
    ) -> Option<impl Deref<Target = T2> + 'a> {
        let derived_domains = self.derived_domains.read().unwrap();
        let derived = derived_domains.get(name)?;

        Some(Arc::downcast::<T2>(derived.clone()).expect("derived domain was not of expected type"))
    }
}
