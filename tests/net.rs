use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::Write;
use std::iter;

use rand::prelude::{IteratorRandom, StdRng};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sn_handover::{
    Ballot, Error, HandoverState, Proposal, PublicKey, Result, SecretKey, SignedVote, Vote, VoteMsg,
};

// dummy proposal for tests
#[derive(Clone, Copy, Debug, Eq, PartialOrd, Ord, PartialEq, Serialize, Deserialize)]
pub struct DummyProposal(pub u64);

impl Proposal for DummyProposal {
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub source: PublicKey,
    pub vote_msg: VoteMsg<DummyProposal>,
}

#[derive(Default, Debug)]
pub struct Net {
    pub procs: Vec<HandoverState<DummyProposal>>,
    pub proposals: BTreeSet<DummyProposal>,
    pub packets: BTreeMap<PublicKey, VecDeque<Packet>>,
    pub delivered_packets: Vec<Packet>,
}

impl Net {
    pub fn with_procs(n: usize, mut rng: &mut StdRng) -> Self {
        let elders_private_k =
            Vec::from_iter((0..n).into_iter().map(|_| SecretKey::random(&mut rng)));
        let gen = 0;

        let mut procs = Vec::from_iter(
            elders_private_k
                .into_iter()
                .map(|sk| HandoverState::from(sk, gen, Default::default())),
        );
        procs.sort_by_key(|p| p.secret_key.public_key());
        Self {
            procs,
            ..Default::default()
        }
    }

    pub fn proc(&self, public_key: PublicKey) -> Option<&HandoverState<DummyProposal>> {
        self.procs.iter().find(|p| p.public_key() == public_key)
    }

    /// Pick a random public key from the set of procs
    pub fn gen_public_key(&self, rng: &mut StdRng) -> PublicKey {
        self.procs
            .iter()
            .choose(rng)
            .map(HandoverState::public_key)
            .unwrap()
    }

    /// Generate a randomized ballot
    pub fn gen_ballot(
        &self,
        recursion: u8,
        faulty: &BTreeSet<PublicKey>,
        rng: &mut StdRng,
    ) -> Ballot<DummyProposal> {
        match rng.gen() || recursion == 0 {
            true => Ballot::Propose(match rng.gen() {
                true => DummyProposal(1),
                false => DummyProposal(0),
            }),
            false => {
                let n_votes = rng.gen::<usize>() % self.procs.len().pow(2);
                let random_votes = BTreeSet::from_iter(
                    iter::repeat_with(|| self.gen_faulty_vote(recursion - 1, faulty, rng))
                        .take(n_votes),
                );
                match rng.gen() {
                    true => Ballot::Merge(random_votes),
                    false => Ballot::SuperMajority(random_votes),
                }
            }
        }
    }

    /// Generate a random faulty vote
    pub fn gen_faulty_vote(
        &self,
        recursion: u8,
        faulty_nodes: &BTreeSet<PublicKey>,
        rng: &mut StdRng,
    ) -> SignedVote<DummyProposal> {
        let faulty_node = faulty_nodes
            .iter()
            .choose(rng)
            .and_then(|pk| self.proc(*pk))
            .unwrap();

        let vote = Vote {
            gen: rng.gen::<u64>() % 7,
            ballot: self.gen_ballot(recursion, faulty_nodes, rng),
        };

        let mut signed_vote = faulty_node.sign_vote(vote).unwrap();
        let node_to_impersonate = self.procs.iter().choose(rng).unwrap().public_key();
        signed_vote.voter = node_to_impersonate;
        signed_vote
    }

    /// Generate a faulty random packet
    pub fn gen_faulty_packet(
        &self,
        recursion: u8,
        faulty: &BTreeSet<PublicKey>,
        rng: &mut StdRng,
    ) -> Packet {
        Packet {
            source: *faulty.iter().choose(rng).unwrap(),
            vote_msg: VoteMsg {
                vote: self.gen_faulty_vote(recursion, faulty, rng),
                dest: self.gen_public_key(rng),
            },
        }
    }

    pub fn genesis(&self) -> Result<PublicKey> {
        self.procs
            .get(0)
            .map(HandoverState::public_key)
            .ok_or(Error::NoMembers)
    }

    pub fn drop_packet_from_source(&mut self, source: PublicKey) {
        self.packets.get_mut(&source).map(VecDeque::pop_front);
    }

    pub fn deliver_packet_from_source(&mut self, source: PublicKey) -> Result<()> {
        let packet = match self.packets.get_mut(&source).map(|ps| ps.pop_front()) {
            Some(Some(p)) => p,
            _ => return Ok(()), // nothing to do
        };
        self.purge_empty_queues();

        let dest = packet.vote_msg.dest;
        println!("delivering {:?}->{:?} {:?}", packet.source, dest, packet);

        self.delivered_packets.push(packet.clone());

        let dest_proc_opt = self.procs.iter_mut().find(|p| p.public_key() == dest);

        let dest_proc = match dest_proc_opt {
            Some(proc) => proc,
            None => {
                println!("[NET] destination proc does not exist, dropping packet");
                return Ok(());
            }
        };

        let dest_members = dest_proc.voters.clone();
        let vote = packet.vote_msg.vote;

        let resp = dest_proc.handle_signed_vote(vote);
        println!("[NET] resp: {:?}", resp);
        match resp {
            Ok(vote_msgs) => {
                let dest_actor = dest_proc.public_key();
                self.enqueue_packets(vote_msgs.into_iter().map(|vote_msg| Packet {
                    source: dest_actor,
                    vote_msg,
                }));
            }
            Err(Error::NonMember {
                public_key: voter,
                members,
            }) => {
                assert_eq!(members, dest_members);
                assert!(
                    !dest_members.contains(&voter),
                    "{:?} should not be in {:?}",
                    source,
                    dest_members
                );
            }
            Err(Error::VoteNotForNextGeneration {
                vote_gen,
                gen,
                pending_gen,
            }) => {
                assert!(vote_gen <= gen || vote_gen > pending_gen);
                assert_eq!(dest_proc.gen, gen);
            }
            Err(err) => return Err(err),
        }

        Ok(())
    }

    pub fn enqueue_packets(&mut self, packets: impl IntoIterator<Item = Packet>) {
        for packet in packets {
            self.packets
                .entry(packet.source)
                .or_default()
                .push_back(packet)
        }
    }

    pub fn drain_queued_packets(&mut self) -> Result<()> {
        while let Some(source) = self.packets.keys().next().cloned() {
            self.deliver_packet_from_source(source)?;
            self.purge_empty_queues();
        }
        Ok(())
    }

    pub fn purge_empty_queues(&mut self) {
        self.packets = core::mem::take(&mut self.packets)
            .into_iter()
            .filter(|(_, queue)| !queue.is_empty())
            .collect();
    }

    pub fn force_join(&mut self, p: PublicKey, q: PublicKey) {
        if let Some(proc) = self.procs.iter_mut().find(|proc| proc.public_key() == p) {
            proc.force_join(q);
        }
    }

    pub fn enqueue_anti_entropy(&mut self, i: usize, j: usize) {
        let i_actor = self.procs[i].public_key();
        let j_actor = self.procs[j].public_key();

        self.enqueue_packets(
            self.procs[j]
                .anti_entropy(i_actor)
                .into_iter()
                .map(|vote_msg| Packet {
                    source: j_actor,
                    vote_msg,
                }),
        );
    }

    pub fn generate_msc(&self, name: &str) -> Result<()> {
        // See: http://www.mcternan.me.uk/mscgen/
        let mut msc = String::from(
            "
msc {\n
  hscale = \"2\";\n
",
        );
        let procs = self
            .procs
            .iter()
            .map(|p| p.public_key())
            .collect::<BTreeSet<_>>() // sort by actor id
            .into_iter()
            .map(|id| format!("{:?}", id))
            .collect::<Vec<_>>()
            .join(",");
        msc.push_str(&procs);
        msc.push_str(";\n");
        for packet in self.delivered_packets.iter() {
            msc.push_str(&format!(
                "{} -> {} [ label=\"{:?}\"];\n",
                packet.source, packet.vote_msg.dest, packet.vote_msg.vote
            ));
        }

        msc.push_str("}\n");

        // Replace process identifiers with friendlier numbers
        // 1, 2, 3 ... instead of i:3b2, i:7def, ...
        for (idx, proc_id) in self.procs.iter().map(HandoverState::public_key).enumerate() {
            let proc_id_as_str = format!("{}", proc_id);
            msc = msc.replace(&proc_id_as_str, &format!("{}", idx + 1));
        }

        let mut msc_file = File::create(name)?;
        msc_file.write_all(msc.as_bytes())?;
        Ok(())
    }
}
