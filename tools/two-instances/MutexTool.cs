// Lists or closes named mutexes held by another process.
// Used by launch-two.ps1 to remove NieR:Automata's single-instance mutex so a second copy can start.
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class MutexTool
{
    [StructLayout(LayoutKind.Sequential)]
    struct HandleEntry
    {
        public IntPtr Object;
        public IntPtr UniqueProcessId;
        public IntPtr HandleValue;
        public uint GrantedAccess;
        public ushort CreatorBackTraceIndex;
        public ushort ObjectTypeIndex;
        public uint HandleAttributes;
        public uint Reserved;
    }

    [DllImport("ntdll.dll")] static extern int NtQuerySystemInformation(int cls, IntPtr buf, int len, out int ret);
    [DllImport("ntdll.dll")] static extern int NtQueryObject(IntPtr h, int cls, IntPtr buf, int len, out int ret);
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool DuplicateHandle(IntPtr srcProc, IntPtr src, IntPtr dstProc, out IntPtr dst, uint access, bool inherit, uint options);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);

    const int SystemExtendedHandleInformation = 64;
    const int ObjectNameInformation = 1;
    const int ObjectTypeInformation = 2;
    const uint PROCESS_DUP_HANDLE = 0x40;
    const uint DUPLICATE_CLOSE_SOURCE = 0x1;
    const uint DUPLICATE_SAME_ACCESS = 0x2;

    // UNICODE_STRING at the start of both OBJECT_NAME_INFORMATION and OBJECT_TYPE_INFORMATION.
    static string ReadUnicodeString(IntPtr buf)
    {
        ushort len = (ushort)Marshal.ReadInt16(buf);
        IntPtr str = Marshal.ReadIntPtr(buf, IntPtr.Size);
        return (len == 0 || str == IntPtr.Zero) ? "" : Marshal.PtrToStringUni(str, len / 2);
    }

    static string Query(IntPtr h, int cls)
    {
        IntPtr buf = Marshal.AllocHGlobal(0x1000);
        try
        {
            int ret;
            return NtQueryObject(h, cls, buf, 0x1000, out ret) == 0 ? ReadUnicodeString(buf) : null;
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    static List<HandleEntry> HandlesOf(int pid)
    {
        int size = 1 << 20;
        while (true)
        {
            IntPtr buf = Marshal.AllocHGlobal(size);
            try
            {
                int ret;
                int status = NtQuerySystemInformation(SystemExtendedHandleInformation, buf, size, out ret);
                if (status == unchecked((int)0xC0000004)) { size = Math.Max(size * 2, ret + 0x10000); continue; }
                if (status != 0) throw new Exception("NtQuerySystemInformation failed: 0x" + status.ToString("X"));
                long count = Marshal.ReadIntPtr(buf).ToInt64();
                int entrySize = Marshal.SizeOf(typeof(HandleEntry));
                var list = new List<HandleEntry>();
                for (long i = 0; i < count; i++)
                {
                    var e = (HandleEntry)Marshal.PtrToStructure(new IntPtr(buf.ToInt64() + IntPtr.Size * 2 + i * entrySize), typeof(HandleEntry));
                    if (e.UniqueProcessId.ToInt64() == pid) list.Add(e);
                }
                return list;
            }
            finally { Marshal.FreeHGlobal(buf); }
        }
    }

    // Returns the names of mutexes in `pid` whose name contains `filter` (all named mutexes if filter is empty).
    // When `close` is true, those handles are closed inside the target process.
    public static List<string> Mutexes(int pid, string filter, bool close)
    {
        return Named(pid, filter, close, new[] { "Mutant" });
    }

    // Same as Mutexes, for any of the given object types (Mutant, Event, Semaphore, Section).
    // Only these types are queried for names, because name queries on pipes can hang.
    public static List<string> Named(int pid, string filter, bool close, string[] types)
    {
        var found = new List<string>();
        IntPtr proc = OpenProcess(PROCESS_DUP_HANDLE, false, pid);
        if (proc == IntPtr.Zero) throw new Exception("OpenProcess failed: " + Marshal.GetLastWin32Error());
        try
        {
            foreach (var e in HandlesOf(pid))
            {
                IntPtr dup;
                if (!DuplicateHandle(proc, e.HandleValue, GetCurrentProcess(), out dup, 0, false, DUPLICATE_SAME_ACCESS)) continue;
                string name = null;
                try
                {
                    string type = Query(dup, ObjectTypeInformation);
                    if (Array.IndexOf(types, type) >= 0)
                    {
                        name = Query(dup, ObjectNameInformation);
                        if (types.Length > 1 && !string.IsNullOrEmpty(name)) name = type + " " + name;
                    }
                }
                finally { CloseHandle(dup); }
                if (string.IsNullOrEmpty(name)) continue;
                if (!string.IsNullOrEmpty(filter) && name.IndexOf(filter, StringComparison.OrdinalIgnoreCase) < 0) continue;
                if (close)
                {
                    IntPtr ignored;
                    if (DuplicateHandle(proc, e.HandleValue, IntPtr.Zero, out ignored, 0, false, DUPLICATE_CLOSE_SOURCE)) name += " (closed)";
                    else name += " (close failed: " + Marshal.GetLastWin32Error() + ")";
                }
                found.Add(name);
            }
        }
        finally { CloseHandle(proc); }
        return found;
    }
}
