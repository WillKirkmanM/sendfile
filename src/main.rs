use clap::Parser;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Transfer a regular file to standard output through the Linux kernel",
    long_about = "Transfer a regular file to standard output using Linux sendfile.\n\nRun on Linux and redirect standard output to a regular file or pipe. Some output destinations, including terminals, may not support sendfile.\n\nExample: sendfile --offset 2 --count 4 input.txt > output.txt"
)]
struct Args {
    #[arg(value_name = "FILE", help = "Regular input file")]
    input: PathBuf,

    #[arg(
        long,
        default_value_t = 0,
        value_name = "BYTES",
        help = "Start at this non-negative byte offset"
    )]
    offset: u64,

    #[arg(
        long,
        value_name = "BYTES",
        help = "Transfer at most this many bytes (default: until EOF)"
    )]
    count: Option<u64>,
}

const MAX_REQUEST: usize = 0x7fff_f000;

fn transfer(
    mut offset: u64,
    count: Option<u64>,
    mut send: impl FnMut(u64, usize) -> io::Result<usize>,
) -> io::Result<()> {
    let mut remaining = count.unwrap_or(u64::MAX);
    while remaining != 0 {
        let request = remaining.min(MAX_REQUEST as u64) as usize;
        match send(offset, request) {
            Ok(0) => break,
            Ok(written) if written <= request => {
                offset = offset.checked_add(written as u64).ok_or_else(|| {
                    io::Error::other("transfer offset exceeds the supported byte range")
                })?;
                remaining -= written as u64;
            }
            Ok(_) => return Err(io::Error::other("sendfile returned an invalid byte count")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn transfer_file(
    input: &mut std::fs::File,
    output: &mut io::StdoutLock<'_>,
    offset: u64,
    count: Option<u64>,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        #[cfg_attr(target_pointer_width = "32", link_name = "sendfile64")]
        fn sendfile(output: i32, input: i32, offset: *mut i64, count: usize) -> isize;
    }

    transfer(offset, count, |offset, request| {
        let mut position = i64::try_from(offset)
            .map_err(|_| io::Error::other("offset exceeds the Linux file position range"))?;
        let written = unsafe {
            sendfile(
                output.as_raw_fd(),
                input.as_raw_fd(),
                &mut position,
                request,
            )
        };
        if written < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn transfer_file(
    input: &mut std::fs::File,
    output: &mut io::StdoutLock<'_>,
    offset: u64,
    count: Option<u64>,
) -> io::Result<()> {
    use io::{Read, Seek, SeekFrom, Write};

    input.seek(SeekFrom::Start(offset))?;
    let mut buffer = [0; 64 * 1024];
    transfer(offset, count, |_, request| {
        let length = request.min(buffer.len());
        let read = input.read(&mut buffer[..length])?;
        output.write_all(&buffer[..read])?;
        Ok(read)
    })?;
    output.flush()
}

fn run(args: Args) -> Result<(), String> {
    use std::fs::{self, File};

    let path = args.input.display();
    let metadata =
        fs::metadata(&args.input).map_err(|error| format!("cannot inspect '{path}': {error}"))?;
    if !metadata.is_file() {
        return Err(format!("'{path}' is not a regular input file"));
    }
    let mut input =
        File::open(&args.input).map_err(|error| format!("cannot open '{path}': {error}"))?;
    let metadata = input
        .metadata()
        .map_err(|error| format!("cannot inspect '{path}': {error}"))?;
    if !metadata.is_file() {
        return Err(format!("'{path}' is not a regular input file"));
    }
    if args.count == Some(0) || args.offset >= metadata.len() {
        return Ok(());
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    transfer_file(&mut input, &mut output, args.offset, args.count)
    .map_err(|error| match error.raw_os_error() {
        #[cfg(target_os = "linux")]
        Some(9 | 19 | 22 | 29 | 38 | 95) => {
            format!("cannot transfer '{path}': standard output does not support sendfile or is not writable: {error}")
        }
        _ => format!("cannot transfer '{path}' to standard output: {error}"),
    })
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sendfile: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_byte_ranges_and_rejects_invalid_values() {
        let args =
            Args::try_parse_from(["sendfile", "--offset", "2", "--count", "4", "input"]).unwrap();
        assert_eq!(args.offset, 2);
        assert_eq!(args.count, Some(4));
        for value in ["invalid", "-1", "1.5", "18446744073709551616"] {
            for option in ["--offset", "--count"] {
                assert!(Args::try_parse_from(["sendfile", option, value, "input"]).is_err());
            }
        }
        let args = Args::try_parse_from(["sendfile", "input"]).unwrap();
        assert_eq!(args.offset, 0);
        assert_eq!(args.count, None);
        assert!(Args::try_parse_from(["sendfile"]).is_err());
        assert!(Args::try_parse_from(["sendfile", "first", "second"]).is_err());
    }

    #[test]
    fn retries_interruptions_and_completes_partial_transfers() {
        let mut calls = Vec::new();
        transfer(2, Some(4), |offset, request| {
            calls.push((offset, request));
            match calls.len() {
                1 => Err(io::ErrorKind::Interrupted.into()),
                2 => Ok(1),
                _ => Ok(3),
            }
        })
        .unwrap();
        assert_eq!(calls, [(2, 4), (2, 4), (3, 3)]);
    }

    #[test]
    fn bounds_requests_and_stops_at_eof() {
        let mut calls = 0;
        transfer(0, None, |offset, request| {
            assert_eq!(request, MAX_REQUEST);
            calls += 1;
            assert_eq!(offset, if calls == 1 { 0 } else { 7 });
            Ok(if calls == 1 { 7 } else { 0 })
        })
        .unwrap();
        assert_eq!(calls, 2);
    }

    #[test]
    fn zero_count_never_calls_sendfile_and_failures_propagate() {
        transfer(u64::MAX, Some(0), |_, _| panic!("unexpected transfer")).unwrap();
        let error = transfer(0, Some(1), |_, _| Err(io::ErrorKind::BrokenPipe.into())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
