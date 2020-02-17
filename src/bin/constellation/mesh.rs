#![allow(clippy::too_many_lines, clippy::erasing_op, clippy::if_not_else)]

use either::Either;
use std::{
	collections::{HashMap, VecDeque}, env, ffi::OsString, fs::File, io, net::{IpAddr, TcpListener, TcpStream}, os::unix::io::FromRawFd, process, sync::mpsc::{sync_channel, SyncSender}, thread, time::{Duration, Instant}
};

use constellation::master_init;
use constellation_internal::{
	abort_on_unwind, abort_on_unwind_1, map_bincode_err, msg::{bincode_deserialize_from, FabricRequest, SchedulerArg}, BufferedStream, Cpu, Mem, Pid, PidInternal, Resources, TrySpawnError
};

use super::MeshRole;

#[derive(Debug)]
pub struct Node {
	mem: Mem,
	cpu: Cpu,
}
impl Node {
	fn fits(&self, process: &Resources) -> bool {
		process.mem <= self.mem && process.cpu <= self.cpu
	}

	fn alloc(&mut self, process: &Resources) {
		assert!(process.cpu <= self.cpu);
		self.mem -= process.mem;
		self.cpu -= process.cpu;
	}

	fn free(&mut self, process: &Resources) {
		self.mem += process.mem;
		self.cpu += process.cpu;
	}
}

pub(super) fn main(role: MeshRole) -> ! {
	master_init(false);
	// let master_pid = constellation::pid();
	match role {
		MeshRole::Master(bind, key, nodes) => {
			let listener = TcpListener::bind(&bind).unwrap();
			let pid = nodes[0].fabric;
			master(listener, Pid::new(pid.ip(), pid.port(), Some(key)), nodes)
		}
		MeshRole::Worker(bind, _key) => {
			let listener = TcpListener::bind(&bind).unwrap();
			loop {
				let accepted = listener.accept();
				if let Ok((stream, addr)) = accepted {
					if bincode::serialize_into::<_, IpAddr>(&mut &stream, &addr.ip()).is_ok() {
						let read = unsafe { File::from_raw_fd(super::BOUND_FD_START) };
						let write = unsafe { File::from_raw_fd(super::BOUND_FD_START + 1) };
						crossbeam::scope(|scope| {
							let _ = scope.spawn(|_| io::copy(&mut &read, &mut &stream));
							let _ = io::copy(&mut &stream, &mut &write);
							process::exit(0);
						})
						.unwrap();
					}
				}
			}
		}
	}
}

fn master(listener: TcpListener, master_pid: Pid, nodes: Vec<super::Node>) -> ! {
	let (sender, receiver) = sync_channel::<
		Either<
			(
				FabricRequest<Vec<u8>, Vec<u8>>,
				SyncSender<Result<Pid, TrySpawnError>>,
				Option<usize>,
			),
			(usize, Either<Pid, Pid>),
		>,
	>(0);

	let mut nodes = nodes
		.into_iter()
		.enumerate()
		.map(|(i, node)| {
			let master = i == 0;
			let fabric = node.fabric;
			let bridge = node.bridge;
			let node = Node {
				mem: node.mem,
				cpu: node.cpu,
			};
			let (sender_a, receiver_a) = sync_channel::<FabricRequest<Vec<u8>, Vec<u8>>>(0);
			let start = Instant::now();
			let sender1 = sender.clone();
			let _ = thread::Builder::new()
				.spawn(abort_on_unwind(move || {
					let (receiver, sender) = (receiver_a, sender1);
					let stream = if !master {
						let stream = loop {
							let err = match TcpStream::connect(&fabric) {
								Ok(stream) => break Ok(stream),
								Err(err) => err,
							};
							if start.elapsed() > Duration::from_secs(60) {
								break Err(err);
							}
							thread::sleep(Duration::from_secs(1));
						}
						.unwrap_or_else(|e| {
							panic!("couldn't connect to node {}: {:?}: {}", i, fabric, e)
						});
						Some(stream)
					} else {
						None
					};
					let (stream_read, stream_write) = if master {
						let read = unsafe { File::from_raw_fd(super::BOUND_FD_START) };
						let write = unsafe { File::from_raw_fd(super::BOUND_FD_START + 1) };
						(Either::Left(read), Either::Left(write))
					} else {
						(
							Either::Right(stream.as_ref().unwrap()),
							Either::Right(stream.as_ref().unwrap()),
						)
					};
					let (mut stream_read, mut stream_write) = (
						BufferedStream::new(stream_read),
						BufferedStream::new(stream_write),
					);
					bincode::serialize_into::<_, IpAddr>(&mut stream_write.write(), &fabric.ip())
						.unwrap();
					if !master {
						let cluster_ip = bincode::deserialize_from::<_, IpAddr>(&mut stream_read)
							.map_err(map_bincode_err)
							.unwrap();
						assert_eq!(cluster_ip, master_pid.addr().ip());
					}
					crossbeam::scope(|scope| {
						let _ = scope.spawn(abort_on_unwind_1(|_spawn| {
							for request in receiver {
								bincode::serialize_into(&mut stream_write.write(), &request)
									.unwrap();
							}
						}));
						while let Ok(done) =
							bincode::deserialize_from::<_, Either<Pid, Pid>>(&mut stream_read)
								.map_err(map_bincode_err)
						{
							sender.send(Either::Right((i, done))).unwrap();
						}
					})
					.unwrap();
				}))
				.unwrap();
			if let Some(bridge) = bridge {
				let sender = sender.clone();
				let _ = thread::Builder::new()
					.spawn(abort_on_unwind(move || {
						#[cfg(feature = "distribute_binaries")]
						let binary = {
							let mut binary = Vec::new();
							let mut file_in = palaver::env::exe().unwrap();
							let _ = std::io::Read::read_to_end(&mut file_in, &mut binary).unwrap();
							binary
						};
						#[cfg(not(feature = "distribute_binaries"))]
						let binary = std::marker::PhantomData;
						let (sender_, receiver) = sync_channel::<Result<Pid, TrySpawnError>>(0);
						sender
							.send(Either::Left((
								FabricRequest {
									block: false,
									resources: Resources {
										mem: 0 * Mem::B,
										cpu: 0 * Cpu::CORE,
									},
									bind: vec![bridge],
									args: vec![
										OsString::from(env::current_exe().unwrap()),
										OsString::from("bridge"),
									],
									vars: Vec::new(),
									binary,
									arg: Vec::new(),
								},
								sender_,
								Some(i),
							)))
							.unwrap();
						let _pid: Pid = receiver.recv().unwrap().unwrap();
						// println!("bridge at {:?}", pid);
					}))
					.unwrap();
			}
			(sender_a, node, fabric.ip(), VecDeque::new())
		})
		.collect::<Vec<_>>();

	let _ = thread::Builder::new()
		.spawn(abort_on_unwind(move || {
			for stream in listener.incoming() {
				// println!("accepted");
				if stream.is_err() {
					continue;
				}
				let stream = stream.unwrap();
				let sender = sender.clone();
				let _ = thread::Builder::new()
					.spawn(abort_on_unwind(move || {
						let (mut stream_read, mut stream_write) =
							(BufferedStream::new(&stream), &stream);
						while let Ok(request) =
							bincode_deserialize_from(&mut stream_read).map_err(map_bincode_err)
						{
							// println!("parsed");
							let (sender_, receiver) = sync_channel::<Result<Pid, TrySpawnError>>(0);
							sender.send(Either::Left((request, sender_, None))).unwrap();
							let pid: Result<Pid, TrySpawnError> = receiver.recv().unwrap();
							// let mut stream_write = stream_write.write();
							if bincode::serialize_into(&mut stream_write, &pid).is_err() {
								break;
							}
						}
					}))
					.unwrap();
			}
		}))
		.unwrap();

	let mut processes: HashMap<(usize, Pid), Resources> = HashMap::new();

	let mut blocked = Vec::new();

	for msg in receiver.iter() {
		match msg {
			Either::Left((mut request, sender, force)) => {
				// println!("spawn {:?}", request.resources);
				let node = force.or_else(|| {
					nodes
						.iter()
						.position(|node| node.1.fits(&request.resources))
				});
				if let Some(node) = node {
					let node = &mut nodes[node];
					node.1.alloc(&request.resources);

					bincode::serialize_into(
						&mut request.arg,
						&SchedulerArg {
							ip: node.2,
							scheduler: master_pid,
						},
					)
					.unwrap();
					node.3.push_back((sender, request.resources.clone()));
					node.0.send(request).unwrap();
				} else {
					// println!(
					// 	"Failing a spawn! Cannot allocate process {:#?} to nodes {:#?}",
					// 	resources, nodes
					// );
					if request.block {
						blocked.push((request, sender));
					} else {
						sender.send(Err(TrySpawnError::NoCapacity)).unwrap();
					}
				}
			}
			Either::Right((node_, Either::Left(pid))) => {
				// println!("init {}:{} ({})", node_, pid, processes.len());
				let node = &mut nodes[node_];
				let (sender, process) = node.3.pop_front().unwrap();
				let x = processes.insert((node_, pid), process);
				assert!(x.is_none());
				sender.send(Ok(pid)).unwrap();
			}
			Either::Right((node, Either::Right(pid))) => {
				let process = processes.remove(&(node, pid)).unwrap();
				// println!("done {}:{} ({})", node, pid, processes.len());
				let node = &mut nodes[node];
				node.1.free(&process);
				blocked = blocked
					.into_iter()
					.filter_map(|(mut request, sender)| {
						if let Some(node) = nodes
							.iter()
							.position(|node| node.1.fits(&request.resources))
						{
							let node = &mut nodes[node];
							node.1.alloc(&request.resources);

							bincode::serialize_into(
								&mut request.arg,
								&SchedulerArg {
									ip: node.2,
									scheduler: master_pid,
								},
							)
							.unwrap();
							node.3.push_back((sender, request.resources.clone()));
							node.0.send(request).unwrap();
							None
						} else {
							Some((request, sender))
						}
					})
					.collect();
			}
		}
	}
	unreachable!()
}
