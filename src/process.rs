use boxed::Box;
use collections::BTreeMap;
use env;
use fmt;
use io::{Result, Read, Write};
use mem;
use os::unix::io::{AsRawFd, FromRawFd, RawFd};
use ops::DerefMut;
use string::{String, ToString};
use core_collections::borrow::ToOwned;
use vec::Vec;

use io::Error;
use syscall::{self, clone, close, dup, execve, pipe2, read, write, waitpid, CLONE_VM, CLONE_VFORK, CLONE_SUPERVISE};
use syscall::Error as SysError;

pub struct ExitStatus {
    status: usize,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.status == 0
    }

    pub fn code(&self) -> Option<i32> {
        Some(self.status as i32)
    }
}

pub struct ChildStdin {
    fd: usize,
}

impl Write for ChildStdin {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        write(self.fd, buf).map_err(|x| Error::from_sys(x))
    }
    fn flush(&mut self) -> Result<()> { Ok(()) }
}

impl Drop for ChildStdin {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

pub struct ChildStdout {
    fd: usize,
}

impl AsRawFd for ChildStdout {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Read for ChildStdout {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        read(self.fd, buf).map_err(|x| Error::from_sys(x))
    }
}

impl Drop for ChildStdout {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

pub struct ChildStderr {
    fd: usize,
}

impl Read for ChildStderr {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        read(self.fd, buf).map_err(|x| Error::from_sys(x))
    }
}

impl Drop for ChildStderr {
    fn drop(&mut self) {
        let _ = close(self.fd);
    }
}

pub struct Child {
    pid: usize,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        let mut status: usize = 0;
        waitpid(self.pid, &mut status, 0).map(|_| ExitStatus { status: status }).map_err(|x| Error::from_sys(x))
    }
}

pub struct Command {
    path: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "{:?}", self.path));
        for arg in &self.args {
            try!(write!(f, " {:?}", arg));
        }
        for (key, val) in &self.env {
            try!(write!(f, " {:?}={:?}", key, val));
        }
        Ok(())
    }
}

impl Command {
    pub fn new(path: &str) -> Command {
        Command {
            path: path.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            stdin: Stdio::inherit(),
            stdout: Stdio::inherit(),
            stderr: Stdio::inherit(),
        }
    }

    pub fn arg(&mut self, arg: &str) -> &mut Command {
        self.args.push(arg.to_owned());
        self
    }

    pub fn env(&mut self, key: &str, val: &str) -> &mut Command {
        self.env.insert(key.to_owned(), val.to_owned());
        self
    }

    pub fn stdin(&mut self, cfg: Stdio) -> &mut Command {
        self.stdin = cfg;
        self
    }

    pub fn stdout(&mut self, cfg: Stdio) -> &mut Command {
        self.stdout = cfg;
        self
    }

    pub fn stderr(&mut self, cfg: Stdio) -> &mut Command {
        self.stderr = cfg;
        self
    }

    pub fn spawn(&mut self) -> Result<Child> {
        self.exec(0)
    }

    /// Spawn this command as a supervised process.
    ///
    /// This means that the system calls will block the process, until being handled by the
    /// parrent. Handling can be done by calling `.id()`, and then using `sys_supervise` to start
    /// supervising this process. Refer to the respective documentation for more information.
    pub fn spawn_supervise(&mut self) -> Result<Child> {
        self.exec(CLONE_SUPERVISE)
    }

    fn exec(&mut self, flags: usize) -> Result<Child> {
        let mut res = Box::new(0);

        let path = if self.path.contains(':') || self.path.contains('/') {
            self.path.to_owned()
        } else {
            let mut path_env = super::env::var("PATH").unwrap_or(".".to_string());

            if ! path_env.ends_with('/') {
                path_env.push('/');
            }

            path_env.push_str(&self.path);

            path_env
        };

        let mut args: Vec<[usize; 2]> = Vec::new();
        for arg in self.args.iter() {
            args.push([arg.as_ptr() as usize, arg.len()]);
        }

        let env = self.env.clone();

        let child_res = res.deref_mut() as *mut usize;
        let child_stderr = self.stderr.inner;
        let child_stdout = self.stdout.inner;
        let child_stdin = self.stdin.inner;
        let child_code = Box::new(move || -> Result<usize> {
            let child_stderr_res = match child_stderr {
                StdioType::Piped(read, write) => {
                    let _ = close(read);
                    let _ = close(2);
                    let dup_res = dup(write).map_err(|x| Error::from_sys(x));
                    let _ = close(write);
                    dup_res
                },
                StdioType::Raw(fd) => {
                    let _ = close(2);
                    let dup_res = dup(fd).map_err(|x| Error::from_sys(x));
                    let _ = close(fd);
                    dup_res
                },
                StdioType::Null => {
                    let _ = close(2);
                    Ok(0)
                },
                _ => Ok(0)
            };

            let child_stdout_res = match child_stdout {
                StdioType::Piped(read, write) => {
                    let _ = close(read);
                    let _ = close(1);
                    let dup_res = dup(write).map_err(|x| Error::from_sys(x));
                    let _ = close(write);
                    dup_res
                },
                StdioType::Raw(fd) => {
                    let _ = close(1);
                    let dup_res = dup(fd).map_err(|x| Error::from_sys(x));
                    let _ = close(fd);
                    dup_res
                },
                StdioType::Null => {
                    let _ = close(1);
                    Ok(0)
                },
                _ => Ok(0)
            };

            let child_stdin_res = match child_stdin {
                StdioType::Piped(read, write) => {
                    let _ = close(write);
                    let _ = close(0);
                    let dup_res = dup(read).map_err(|x| Error::from_sys(x));
                    let _ = close(read);
                    dup_res
                },
                StdioType::Raw(fd) => {
                    let _ = close(0);
                    let dup_res = dup(fd).map_err(|x| Error::from_sys(x));
                    let _ = close(fd);
                    dup_res
                },
                StdioType::Null => {
                    let _ = close(0);
                    Ok(0)
                },
                _ => Ok(0)
            };

            let _ = try!(child_stderr_res);
            let _ = try!(child_stdout_res);
            let _ = try!(child_stdin_res);

            for (key, val) in env.iter() {
                env::set_var(key, val);
            }

            execve(&path, &args).map_err(|x| Error::from_sys(x))
        });

        match unsafe { clone(flags) } {
            Ok(0) => {
                let error = child_code();

                unsafe { *child_res = SysError::mux(error.map_err(|x| x.into_sys())); }

                loop {
                    let _ = syscall::exit(127);
                }
            },
            Ok(pid) => {
                // Must forget child_code to prevent double free
                mem::forget(child_code);
                if let Err(err) = SysError::demux(*res) {
                    match self.stdin.inner {
                        StdioType::Piped(read, write) => {
                            let _ = close(read);
                            let _ = close(write);
                        },
                        StdioType::Raw(fd) => {
                            let _ = close(fd);
                        },
                        _ => ()
                    }

                    match self.stdout.inner {
                        StdioType::Piped(read, write) => {
                            let _ = close(write);
                            let _ = close(read);
                        },
                        StdioType::Raw(fd) => {
                            let _ = close(fd);
                        },
                        _ => ()
                    }

                    match self.stderr.inner {
                        StdioType::Piped(read, write) => {
                            let _ = close(write);
                            let _ = close(read);
                        },
                        StdioType::Raw(fd) => {
                            let _ = close(fd);
                        },
                        _ => ()
                    }

                    Err(Error::from_sys(err))
                } else {
                    Ok(Child {
                        pid: pid,
                        stdin: match self.stdin.inner {
                            StdioType::Piped(read, write) => {
                                let _ = close(read);
                                Some(ChildStdin {
                                    fd: write
                                })
                            },
                            StdioType::Raw(fd) => {
                                let _ = close(fd);
                                None
                            },
                            _ => None
                        },
                        stdout: match self.stdout.inner {
                            StdioType::Piped(read, write) => {
                                let _ = close(write);
                                Some(ChildStdout {
                                    fd: read
                                })
                            },
                            StdioType::Raw(fd) => {
                                let _ = close(fd);
                                None
                            },
                            _ => None
                        },
                        stderr: match self.stderr.inner {
                            StdioType::Piped(read, write) => {
                                let _ = close(write);
                                Some(ChildStderr {
                                    fd: read
                                })
                            },
                            StdioType::Raw(fd) => {
                                let _ = close(fd);
                                None
                            },
                            _ => None
                        }
                    })
                }
            }
            Err(err) => Err(Error::from_sys(err))
        }
    }
}

#[derive(Copy, Clone)]
enum StdioType {
    Piped(usize, usize),
    Raw(usize),
    Inherit,
    Null,
}

pub struct Stdio {
    inner: StdioType,
}

impl Stdio {
    pub fn piped() -> Stdio {
        let mut fds = [0; 2];
        if pipe2(&mut fds, 0).is_ok() {
            Stdio {
                inner: StdioType::Piped(fds[0], fds[1])
            }
        } else {
            Stdio::null()
        }
    }

    pub fn inherit() -> Stdio {
        Stdio {
            inner: StdioType::Inherit
        }
    }

    pub fn null() -> Stdio {
        Stdio {
            inner: StdioType::Null
        }
    }
}

impl FromRawFd for Stdio {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Stdio {
            inner: StdioType::Raw(fd)
        }
    }
}

pub fn exit(code: i32) -> ! {
    loop {
        let _ = syscall::exit(code as usize);
    }
}
