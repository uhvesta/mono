import Darwin
import Foundation

/// Captures and symbolicates the call stack of another thread (the main
/// thread) from a watchdog thread by suspending it and walking the
/// frame-pointer chain.
///
/// Deadlock safety: while the target thread is suspended we touch **no**
/// heap allocator — only a caller-provided buffer and Mach syscalls
/// (`thread_get_state`, `vm_read_overwrite`). If the suspended main
/// thread happened to hold the malloc lock, allocating here would
/// deadlock the watchdog. So `capture` returns raw return addresses
/// only; `symbolicate` (which allocates Strings via `dladdr`) runs
/// *after* the thread is resumed.
enum MainThreadBacktrace {
    /// Suspend `thread`, walk its frame pointers, resume it, and return
    /// the raw return addresses (innermost first). Empty on failure.
    static func capture(thread: thread_t, maxFrames: Int = 64) -> [UInt] {
        guard maxFrames > 0 else { return [] }
        let buffer = UnsafeMutablePointer<UInt>.allocate(capacity: maxFrames)
        defer { buffer.deallocate() }

        guard thread_suspend(thread) == KERN_SUCCESS else { return [] }
        let count = fillFrames(thread: thread, into: buffer, maxFrames: maxFrames)
        _ = thread_resume(thread)

        return Array(UnsafeBufferPointer(start: buffer, count: count))
    }

    /// Resolve each address to `image  0x…  symbol + offset` via
    /// `dladdr`. Allocates, so call only after the target thread has
    /// been resumed.
    static func symbolicate(_ addresses: [UInt]) -> [String] {
        addresses.enumerated().map { idx, addr in
            var info = Dl_info()
            guard dladdr(UnsafeRawPointer(bitPattern: addr), &info) != 0 else {
                return formatFrame(
                    index: idx,
                    image: "???",
                    address: addr,
                    symbol: hex(addr),
                    offset: 0
                )
            }
            let image = info.dli_fname
                .flatMap { String(validatingCString: $0) }
                .map { ($0 as NSString).lastPathComponent } ?? "???"
            let symbol = info.dli_sname
                .flatMap { String(validatingCString: $0) } ?? hex(addr)
            let symAddr = UInt(bitPattern: info.dli_saddr)
            let offset = (symAddr != 0 && addr >= symAddr) ? addr - symAddr : 0
            return formatFrame(
                index: idx,
                image: image,
                address: addr,
                symbol: symbol,
                offset: offset
            )
        }
    }

    /// One frame rendered in the column layout `Thread.callStackSymbols`
    /// uses. Pure, so the format is unit-testable without a live stack.
    static func formatFrame(
        index: Int,
        image: String,
        address: UInt,
        symbol: String,
        offset: UInt
    ) -> String {
        let idxCol = String(index).padding(toLength: 3, withPad: " ", startingAt: 0)
        let imgCol = image.padding(toLength: 30, withPad: " ", startingAt: 0)
        let addrHex = "0x" + String(format: "%016lx", address)
        return "\(idxCol) \(imgCol) \(addrHex) \(symbol) + \(offset)"
    }

    // MARK: - Frame-pointer walk (suspended-thread phase, no allocation)

    private static func fillFrames(
        thread: thread_t,
        into buffer: UnsafeMutablePointer<UInt>,
        maxFrames: Int
    ) -> Int {
        var pc: UInt = 0
        var fp: UInt = 0
        guard readThreadState(thread, pc: &pc, fp: &fp) else { return 0 }

        var idx = 0
        if pc != 0 {
            buffer[idx] = pc
            idx += 1
        }

        let wordSize = UInt(MemoryLayout<UInt>.size)
        var current = fp
        while idx < maxFrames, current != 0, current % wordSize == 0 {
            // A standard frame stores {saved frame pointer, return address}
            // at [fp] and [fp + wordSize].
            var nextFp: UInt = 0
            var retAddr: UInt = 0
            guard readWord(current, &nextFp),
                  readWord(current + wordSize, &retAddr) else { break }
            if retAddr == 0 { break }
            buffer[idx] = retAddr
            idx += 1
            // The stack grows down, so each caller frame sits at a higher
            // address. Anything else means a corrupt/leaf frame — stop.
            if nextFp <= current { break }
            current = nextFp
        }
        return idx
    }

    /// Read one machine word at `addr` from our own task safely. Uses
    /// `vm_read_overwrite` (a syscall, no allocation) so a corrupt frame
    /// pointer yields a failure rather than a crash.
    private static func readWord(_ addr: UInt, _ out: inout UInt) -> Bool {
        var outSize: vm_size_t = 0
        let size = vm_size_t(MemoryLayout<UInt>.size)
        let kr = withUnsafeMutablePointer(to: &out) { dst -> kern_return_t in
            vm_read_overwrite(
                mach_task_self_,
                vm_address_t(addr),
                size,
                vm_address_t(UInt(bitPattern: UnsafeMutableRawPointer(dst))),
                &outSize
            )
        }
        return kr == KERN_SUCCESS && outSize == size
    }

    private static func readThreadState(
        _ thread: thread_t,
        pc: inout UInt,
        fp: inout UInt
    ) -> Bool {
        #if arch(arm64)
        var state = arm_thread_state64_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<arm_thread_state64_t>.stride / MemoryLayout<natural_t>.stride
        )
        let kr = withUnsafeMutablePointer(to: &state) {
            $0.withMemoryRebound(to: natural_t.self, capacity: Int(count)) {
                thread_get_state(thread, thread_state_flavor_t(ARM_THREAD_STATE64), $0, &count)
            }
        }
        guard kr == KERN_SUCCESS else { return false }
        pc = UInt(state.__pc)
        fp = UInt(state.__fp)
        return true
        #elseif arch(x86_64)
        var state = x86_thread_state64_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<x86_thread_state64_t>.stride / MemoryLayout<natural_t>.stride
        )
        let kr = withUnsafeMutablePointer(to: &state) {
            $0.withMemoryRebound(to: natural_t.self, capacity: Int(count)) {
                thread_get_state(thread, thread_state_flavor_t(x86_THREAD_STATE64), $0, &count)
            }
        }
        guard kr == KERN_SUCCESS else { return false }
        pc = UInt(state.__rip)
        fp = UInt(state.__rbp)
        return true
        #else
        return false
        #endif
    }

    private static func hex(_ v: UInt) -> String {
        "0x" + String(v, radix: 16)
    }
}
