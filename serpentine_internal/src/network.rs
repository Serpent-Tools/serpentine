//! A network container topology.
//!
//! Serpentines network toplogies are always directioanl trees, containing no loops or multi-parent
//! nodes.
//!
//! Most toplogies are either of the `A` or `A --> B` variant.
//! But serpentine does support arbitrary toplogies, for example `A --> B --> C` or
//! ```mermaid
//! flowchart
//!    A --> B --> C
//!    A --> D
//!    B --> E
//! ```
//!
//! NOTE: Even if a service state is attached at multiple places in the tree, each attach is a
//! seperate instance of the service, and they do not share state. Hence for example in the above
//! `D` and `E` might be the "same" service template, but it will spawn two different processes.

use std::hash::Hash;
use std::net::Ipv4Addr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::WireFormat;

/// A generic network topology struct, for holding the various kinds of network toplogiy forms.
///
/// `Eq` and `Ord` only consider the abstract topology structure, not the data.
#[derive(Debug, Clone)]
#[must_use]
pub struct Topology<T> {
    /// This specific network kinds data
    data: T,
    /// Sorted children nodes of this tree.
    children: Vec<Topology<T>>,
}

impl<T> Topology<T> {
    /// Create a new network topology with one root.
    pub fn new(data: T) -> Self {
        Self {
            data,
            children: Vec::new(),
        }
    }

    /// create a new netork topology with one root and children.
    pub fn with_children(data: T, children: Vec<Topology<T>>) -> Self {
        let mut topology = Self { data, children };
        topology.children.sort();
        topology
    }

    /// Add a child to this topology
    pub fn add_child(&mut self, child: Topology<T>) {
        self.children.push(child);
        self.children.sort();
    }

    /// Get the data of this topology node.
    pub fn get_data(&self) -> &T {
        &self.data
    }

    /// Split this topology into its data and children.
    pub fn into_parts(self) -> (T, Vec<Topology<T>>) {
        (self.data, self.children)
    }

    /// map a function over the data of this topology, without changing the topology structure.
    /// Useful for converting between different kinds of toplogies, for example from `ConcreteToplogy` to `AbstractToplogy`.
    pub fn map_data<U>(self, func: impl Fn(T) -> U) -> Topology<U> {
        let data = func(self.data);
        let children = self
            .children
            .into_iter()
            .map(|child| child.map_data(&func))
            .collect();
        Topology { data, children }
    }

    /// Same as `map_data` but using a reference instead.
    pub fn map_data_ref<U>(&self, func: impl Fn(&T) -> U + Copy) -> Topology<U> {
        let data = func(&self.data);
        let children = self
            .children
            .iter()
            .map(|child| child.map_data_ref(func))
            .collect();
        Topology { data, children }
    }

    /// Zip this topology with another one, combining their data.
    ///
    /// This is intended to be used for combining equal topoligies, but for unequal topoligies the
    /// behaviour is not defined, but does not panic.
    pub fn zip<U>(self, other: Topology<U>) -> Topology<(T, U)> {
        let data = (self.data, other.data);
        let children = self
            .children
            .into_iter()
            .zip(other.children)
            .map(|(child1, child2)| child1.zip(child2))
            .collect();
        Topology { data, children }
    }

    /// Compare topoligies for equality, including data.
    ///
    /// WARN: This is non-deterministic when sibiling topoligies have the same shape.
    pub fn eq_with_data(&self, other: &Self) -> bool
    where
        T: PartialEq,
    {
        if self.data != other.data {
            return false;
        }
        if self.children.len() != other.children.len() {
            return false;
        }
        for (child1, child2) in self.children.iter().zip(&other.children) {
            if !child1.eq_with_data(child2) {
                return false;
            }
        }
        true
    }

    /// Return a flattend iterator of all the data in this topology, in no specific order.
    pub fn flat_data(self) -> TopologyIter<T> {
        TopologyIter { stack: vec![self] }
    }
}

impl<T> PartialEq for Topology<T> {
    fn eq(&self, other: &Self) -> bool {
        self.children == other.children
    }
}

impl<T> Eq for Topology<T> {}

impl<T> PartialOrd for Topology<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Topology<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.children.cmp(&other.children)
    }
}

/// A iterator over the data in a toplogy in no specific order.
pub struct TopologyIter<T> {
    /// The stack of topoligies to visit, the top of the stack is the next one to visit.
    stack: Vec<Topology<T>>,
}

impl<T> IntoIterator for Topology<T> {
    type Item = T;
    type IntoIter = TopologyIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.flat_data()
    }
}

impl<T> Iterator for TopologyIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let topology = self.stack.pop()?;
        for child in topology.children {
            self.stack.push(child);
        }
        Some(topology.data)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.stack.len(), None)
    }
}

impl<T: WireFormat> WireFormat for Topology<T> {
    async fn write(
        self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin + Send),
    ) -> crate::Result<()> {
        self.data.write(writer).await?;

        super::write_u64_variable_length(writer, self.children.len() as u64).await?;
        for child in self.children {
            // This didnt trigger the recursion detection (for some reason),
            // But did trigger layout calculation cycle on call sites.
            Box::pin(child.write(writer)).await?;
        }

        Ok(())
    }

    async fn read(reader: &mut (impl tokio::io::AsyncRead + Unpin + Send)) -> crate::Result<Self> {
        let data = T::read(reader).await?;

        let children_len = super::read_u64_length_encoded(reader).await?;
        let mut children = Vec::new();
        for _ in 0..children_len {
            // This didnt trigger the recursion detection (for some reason),
            // But did trigger layout calculation cycle on call sites.
            children.push(Box::pin(Self::read(reader)).await?);
        }

        Ok(Self { data, children })
    }
}

/// A abstract topology only contains the structure of the tree, without any data.
/// This kind also implements `Hash`.
pub type AbstractTopology = Topology<()>;

impl Hash for AbstractTopology {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.children.hash(state);
    }
}

/// A CNI adapter attached to a namespace.
#[derive(Debug, Clone)]
pub struct Adapter {
    /// The interface name, for example "lo", "eth0", or a bridge UUID.
    pub ifname: Box<str>,
    /// The CNI config JSON used to create this adapter, needed for teardown.
    pub config_json: Box<str>,
}

impl WireFormat for Adapter {
    async fn write(
        self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin + Send),
    ) -> crate::Result<()> {
        super::write_length_prefixed(writer, self.ifname.as_bytes()).await?;
        super::write_length_prefixed(writer, self.config_json.as_bytes()).await?;
        Ok(())
    }

    async fn read(reader: &mut (impl tokio::io::AsyncRead + Unpin + Send)) -> crate::Result<Self> {
        let ifname = super::read_length_prefixed_string(reader).await?.into();
        let config_json = super::read_length_prefixed_string(reader).await?.into();
        Ok(Self { ifname, config_json })
    }
}

/// A description of a network namespace created by the sidecar.
#[derive(Debug, Clone)]
pub struct Namespace {
    /// The path of the namespace, for example "/.../netns-1234".
    pub path: Box<str>,
    /// The ip address the process in this namespace will have.
    pub ip: Ipv4Addr,
    /// The CNI network adapters attached to this namespace, needed for teardown.
    pub adapters: Vec<Adapter>,
}

impl WireFormat for Namespace {
    async fn write(
        self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin + Send),
    ) -> crate::Result<()> {
        super::write_length_prefixed(writer, self.path.as_bytes()).await?;

        let ip_bytes = self.ip.octets();
        for byte in ip_bytes {
            writer.write_u8(byte).await?;
        }

        super::write_u64_variable_length(writer, self.adapters.len() as u64).await?;
        for adapter in self.adapters {
            adapter.write(writer).await?;
        }

        Ok(())
    }

    async fn read(reader: &mut (impl tokio::io::AsyncRead + Unpin + Send)) -> crate::Result<Self> {
        let path = super::read_length_prefixed_string(reader).await?;
        let mut ip_bytes = [0u8; 4];
        for byte in &mut ip_bytes {
            *byte = reader.read_u8().await?;
        }
        let ip = Ipv4Addr::from(ip_bytes);

        let adapter_count = super::read_u64_length_encoded(reader).await?;
        let mut adapters = Vec::new();
        for _ in 0..adapter_count {
            adapters.push(Adapter::read(reader).await?);
        }

        Ok(Self {
            path: path.into(),
            ip,
            adapters,
        })
    }
}

/// A concrete network topology with resolved namespaces.
pub type ConcreteTopology = Topology<Namespace>;

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;

    use super::*;

    #[test]
    fn topology_equality() {
        let mut topology1 = Topology::new("A");
        topology1.add_child(Topology::new("B"));
        topology1.add_child(Topology::new("C"));

        let mut topology2 = Topology::new("X");
        topology2.add_child(Topology::new("Y"));
        topology2.add_child(Topology::new("Z"));

        assert_eq!(topology1, topology2);
    }

    #[test]
    fn topology_inequality() {
        let mut topology1 = Topology::new("A");
        topology1.add_child(Topology::new("B"));
        topology1.add_child(Topology::new("C"));

        let mut topology2 = Topology::new("X");
        topology2.add_child(Topology::new("Y"));

        assert_ne!(topology1, topology2);
    }

    #[test]
    fn topology_add_child_order_doesnt_matter() {
        let mut sub1 = Topology::new("B");
        sub1.add_child(Topology::new("D"));
        let mut topology1 = Topology::new("A");
        topology1.add_child(sub1);
        topology1.add_child(Topology::new("C"));

        let mut sub2 = Topology::new("Y");
        sub2.add_child(Topology::new("D"));
        let mut topology2 = Topology::new("X");
        topology2.add_child(Topology::new("C"));
        topology2.add_child(sub2);

        assert_eq!(topology1, topology2);
    }

    #[test]
    fn topology_hash() {
        let mut topology1 = AbstractTopology::new(());
        topology1.add_child(AbstractTopology::new(()));
        topology1.add_child(AbstractTopology::new(()));

        let mut topology2 = AbstractTopology::new(());
        topology2.add_child(AbstractTopology::new(()));
        topology2.add_child(AbstractTopology::new(()));

        let mut hasher1 = DefaultHasher::new();
        topology1.hash(&mut hasher1);
        let hash1 = hasher1.finish();

        let mut hasher2 = DefaultHasher::new();
        topology2.hash(&mut hasher2);
        let hash2 = hasher2.finish();

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn topology_zip() {
        let mut topology1 = Topology::new("A");
        let mut child1 = Topology::new("B");
        child1.add_child(Topology::new("D"));
        topology1.add_child(child1);
        topology1.add_child(Topology::new("C"));

        let mut topology2 = Topology::new("X");
        let mut child2 = Topology::new("Y");
        child2.add_child(Topology::new("Z"));
        topology2.add_child(child2);
        topology2.add_child(Topology::new("W"));

        let zipped = topology1.zip(topology2);
        let expected = Topology::with_children(
            ("A", "X"),
            vec![
                Topology::with_children(("B", "Y"), vec![Topology::new(("D", "Z"))]),
                Topology::new(("C", "W")),
            ],
        );

        assert!(zipped.eq_with_data(&expected));
    }
}
