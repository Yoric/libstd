use boxed::Box;
use fs::File;
use io::{Result, Read, Write};
use ops::DerefMut;
use string::{String, ToString};
use vec::Vec;

use system::error::Error;
use system::syscall::{sys_clone, sys_close, sys_dup, sys_execve, sys_exit, sys_pipe2, sys_waitpid, CLONE_VM, CLONE_VFORK};

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
    inner: File,
}

impl Write for ChildStdin {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.inner.write(buf)
    }
}

pub struct ChildStdout {
    inner: File,
}

impl Read for ChildStdout {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.inner.read(buf)
    }
}

pub struct ChildStderr {
    inner: File,
}

impl Read for ChildStderr {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.inner.read(buf)
    }
}

pub struct Child {
    pid: isize,
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
        let result = unsafe { sys_waitpid(self.pid, &mut status, 0) } as isize;
        if result >= 0 {
            Ok(ExitStatus { status: status })
        } else {
            Err(Error::new(-result))
        }
    }
}

pub struct Command {
    pub path: String,
    pub args: Vec<String>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
}

impl Command {
    pub fn new(path: &str) -> Command {
        Command {
            path: path.to_string(),
            args: Vec::new(),
            stdin: Stdio::inherit(),
            stdout: Stdio::inherit(),
            stderr: Stdio::inherit(),
        }
    }

    pub fn arg(&mut self, arg: &str) -> &mut Command {
        self.args.push(arg.to_string());
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
        let mut res = Box::new(0);

        let path_c = self.path.to_string() + "\0";

        let mut args_vec: Vec<String> = Vec::new();
        for arg in self.args.iter() {
            args_vec.push(arg.to_string() + "\0");
        }

        let mut args_c: Vec<*const u8> = Vec::new();
        for arg_vec in args_vec.iter() {
            args_c.push(arg_vec.as_ptr());
        }
        args_c.push(0 as *const u8);

        let child_res = res.deref_mut() as *mut usize;
        let child_stderr = self.stderr.inner;
        let child_stdout = self.stdout.inner;
        let child_stdin = self.stdin.inner;
        let child_code = move || -> ! {
            unsafe {
                match child_stderr {
                    StdioType::Piped(read, write) => {
                        sys_close(2);
                        sys_dup(write);
                        sys_close(read);
                    },
                    StdioType::Null => {
                        sys_close(2);
                    },
                    _ => ()
                }

                match child_stdout {
                    StdioType::Piped(read, write) => {
                        sys_close(1);
                        sys_dup(write);
                        sys_close(read);
                    },
                    StdioType::Null => {
                        sys_close(1);
                    },
                    _ => ()
                }

                match child_stdin {
                    StdioType::Piped(read, write) => {
                        sys_close(0);
                        sys_dup(read);
                        sys_close(write);
                    },
                    StdioType::Null => {
                        sys_close(0);
                    },
                    _ => ()
                }

                *child_res = sys_execve(path_c.as_ptr(), args_c.as_ptr());
                loop {
                    sys_exit(127);
                }
            }
        };

        let parent_code = move |pid: isize| -> Result<Child> {
            if let Err(err) = Error::demux(*res) {
                Err(err)
            } else {
                Ok(Child {
                    pid: pid,
                    stdin: match self.stdin.inner {
                        StdioType::Piped(read, write) => {
                            unsafe { sys_close(read); }
                            Some(ChildStdin {
                                inner: try!(unsafe { File::from_fd(write) })
                            })
                        },
                        _ => None
                    },
                    stdout: match self.stdout.inner {
                        StdioType::Piped(read, write) => {
                            unsafe { sys_close(write); }
                            Some(ChildStdout {
                                inner: try!(unsafe { File::from_fd(read) })
                            })
                        },
                        _ => None
                    },
                    stderr: match self.stderr.inner {
                        StdioType::Piped(read, write) => {
                            unsafe { sys_close(write); }
                            Some(ChildStderr {
                                inner: try!(unsafe { File::from_fd(read) })
                            })
                        },
                        _ => None
                    }
                })
            }
        };

        let pid = unsafe { sys_clone(CLONE_VM | CLONE_VFORK) } as isize;
        if pid == 0 {
            child_code()
        } else if pid > 0 {
            parent_code(pid)
        } else {
            Err(Error::new(-pid))
        }
    }
}

#[derive(Copy, Clone)]
enum StdioType {
    Piped(usize, usize),
    Inherit,
    Null,
}

pub struct Stdio {
    inner: StdioType,
}

impl Stdio {
    pub fn piped() -> Stdio {
        let mut fds = [0; 2];
        if Error::demux(unsafe { sys_pipe2(fds.as_mut_ptr(), 0) }).is_ok() {
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

pub fn exit(code: i32) -> ! {
    loop {
        unsafe { sys_exit(code as isize) };
    }
}
