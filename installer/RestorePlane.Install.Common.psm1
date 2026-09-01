Set-StrictMode -Version Latest

if (-not ("RestorePlane.Installation.NativePathLock" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Globalization;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace RestorePlane.Installation
{
    public sealed class PathLockSession : IDisposable
    {
        private readonly object sync = new object();
        private readonly List<SafeFileHandle> handles = new List<SafeFileHandle>();
        private bool disposed;

        public int LockedPathCount
        {
            get
            {
                lock (sync)
                {
                    return handles.Count;
                }
            }
        }

        internal void Add(SafeFileHandle handle)
        {
            lock (sync)
            {
                if (disposed)
                {
                    handle.Dispose();
                    throw new ObjectDisposedException("PathLockSession");
                }
                handles.Add(handle);
            }
        }

        public void Dispose()
        {
            Dispose(true);
            GC.SuppressFinalize(this);
        }

        private void Dispose(bool disposing)
        {
            lock (sync)
            {
                if (disposed)
                {
                    return;
                }
                disposed = true;
                for (int index = handles.Count - 1; index >= 0; index--)
                {
                    handles[index].Dispose();
                }
                handles.Clear();
            }
        }

        ~PathLockSession()
        {
            Dispose(false);
        }
    }

    public static class NativePathLock
    {
        private const uint FileShareRead = 0x00000001;
        private const uint FileShareWrite = 0x00000002;
        private const uint FileShareDelete = 0x00000004;
        private const uint FileReadDataOrListDirectory = 0x00000001;
        private const uint DeleteAccess = 0x00010000;
        private const uint OpenExisting = 3;
        private const uint FileAttributeDirectory = 0x00000010;
        private const uint FileAttributeReparsePoint = 0x00000400;
        private const uint FileFlagBackupSemantics = 0x02000000;
        private const uint FileFlagOpenReparsePoint = 0x00200000;
        private const int FileRenameInfo = 3;
        private const int FileAttributeTagInfo = 9;
        private const int FileIdInfo = 18;
        private const int MaxPathCharacters = 32767;

        [StructLayout(LayoutKind.Sequential)]
        private struct FileAttributeTagInformation
        {
            internal uint FileAttributes;
            internal uint ReparseTag;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct FileIdInformation
        {
            internal ulong VolumeSerialNumber;
            internal ulong FileIdPart0;
            internal ulong FileIdPart1;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern SafeFileHandle CreateFileW(
            string fileName,
            uint desiredAccess,
            uint shareMode,
            IntPtr securityAttributes,
            uint creationDisposition,
            uint flagsAndAttributes,
            IntPtr templateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetFileInformationByHandleEx(
            SafeFileHandle file,
            int fileInformationClass,
            out FileAttributeTagInformation fileInformation,
            uint bufferSize);

        [DllImport(
            "kernel32.dll",
            EntryPoint = "GetFileInformationByHandleEx",
            SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetFileIdInformationByHandleEx(
            SafeFileHandle file,
            int fileInformationClass,
            out FileIdInformation fileInformation,
            uint bufferSize);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern uint GetFinalPathNameByHandleW(
            SafeFileHandle file,
            StringBuilder filePath,
            uint filePathLength,
            uint flags);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool SetFileInformationByHandle(
            SafeFileHandle file,
            int fileInformationClass,
            IntPtr fileInformation,
            uint bufferSize);

        public static PathLockSession AcquirePathMutationLock(string path)
        {
            string target = NormalizePath(path);
            PathLockSession session = new PathLockSession();
            try
            {
                AddAncestorIdentityLocks(target, session);
                bool targetIsDirectory;
                OpenVerifiedPath(
                    target,
                    FileReadDataOrListDirectory,
                    FileShareRead,
                    session,
                    out targetIsDirectory);
                return session;
            }
            catch
            {
                session.Dispose();
                throw;
            }
        }

        public static PathLockSession AcquireTree(string rootPath, int maxEntries)
        {
            if (maxEntries < 1)
            {
                throw new ArgumentOutOfRangeException("maxEntries");
            }

            string root = NormalizePath(rootPath);
            PathLockSession session = new PathLockSession();
            try
            {
                AddAncestorIdentityLocks(root, session);
                bool rootIsDirectory;
                OpenVerifiedPath(
                    root,
                    FileReadDataOrListDirectory,
                    FileShareRead,
                    session,
                    out rootIsDirectory);
                if (!rootIsDirectory)
                {
                    throw new IOException("The install tree root is not a directory.");
                }

                int visited = 1;
                Queue<string> directories = new Queue<string>();
                directories.Enqueue(root);
                while (directories.Count > 0)
                {
                    string directory = directories.Dequeue();
                    foreach (string child in Directory.EnumerateFileSystemEntries(directory))
                    {
                        visited++;
                        if (visited > maxEntries)
                        {
                            throw new IOException("The install tree lock limit was exceeded.");
                        }

                        bool childIsDirectory;
                        OpenVerifiedPath(
                            child,
                            FileReadDataOrListDirectory,
                            FileShareRead,
                            session,
                            out childIsDirectory);
                        if (childIsDirectory)
                        {
                            directories.Enqueue(child);
                        }
                    }
                }
                return session;
            }
            catch
            {
                session.Dispose();
                throw;
            }
        }

        public static void MoveDirectoryByHandle(
            string sourcePath,
            string destinationPath)
        {
            string source = NormalizePath(sourcePath);
            string destination = NormalizePath(destinationPath);
            if (String.Equals(
                    source,
                    destination,
                    StringComparison.OrdinalIgnoreCase))
            {
                throw new IOException("The source and destination are identical.");
            }

            string sourceRoot = Path.GetPathRoot(source);
            string destinationRoot = Path.GetPathRoot(destination);
            if (!String.Equals(
                    sourceRoot,
                    destinationRoot,
                    StringComparison.OrdinalIgnoreCase))
            {
                throw new IOException("Directory rename must remain on one volume.");
            }

            string destinationParent = Path.GetDirectoryName(destination);
            string sourceParent = Path.GetDirectoryName(source);
            if (String.IsNullOrWhiteSpace(destinationParent))
            {
                throw new IOException("The destination parent is invalid.");
            }
            if (String.IsNullOrWhiteSpace(sourceParent))
            {
                throw new IOException("The source parent is invalid.");
            }
            destinationParent = NormalizePath(destinationParent);
            sourceParent = NormalizePath(sourceParent);

            SafeFileHandle sourceParentHandle = null;
            SafeFileHandle destinationParentHandle = null;
            SafeFileHandle sourceHandle = null;
            SafeFileHandle destinationHandle = null;
            PathLockSession ancestorLocks = new PathLockSession();
            bool renameCompleted = false;
            try
            {
                AddAncestorIdentityLocks(sourceParent, ancestorLocks);
                AddAncestorIdentityLocks(destinationParent, ancestorLocks);

                bool sourceParentIsDirectory;
                sourceParentHandle = OpenVerifiedHandle(
                    sourceParent,
                    FileReadDataOrListDirectory,
                    FileShareRead | FileShareWrite | FileShareDelete,
                    out sourceParentIsDirectory);
                if (!sourceParentIsDirectory)
                {
                    throw new IOException("The source parent is not a directory.");
                }

                bool destinationParentIsDirectory;
                destinationParentHandle = OpenVerifiedHandle(
                    destinationParent,
                    FileReadDataOrListDirectory,
                    FileShareRead | FileShareWrite | FileShareDelete,
                    out destinationParentIsDirectory);
                if (!destinationParentIsDirectory)
                {
                    throw new IOException("The destination parent is not a directory.");
                }

                bool sourceIsDirectory;
                sourceHandle = OpenVerifiedHandle(
                    source,
                    FileReadDataOrListDirectory | DeleteAccess,
                    FileShareRead | FileShareWrite | FileShareDelete,
                    out sourceIsDirectory);
                if (!sourceIsDirectory)
                {
                    throw new IOException("The rename source is not a directory.");
                }
                FileIdInformation sourceIdentity = GetFileIdentity(sourceHandle);

                RenameHandle(sourceHandle, destination);
                renameCompleted = true;
                bool destinationIsDirectory;
                destinationHandle = OpenVerifiedHandle(
                    destination,
                    FileReadDataOrListDirectory,
                    FileShareRead | FileShareWrite | FileShareDelete,
                    out destinationIsDirectory);
                if (!destinationIsDirectory ||
                    !SameFileIdentity(
                        sourceIdentity,
                        GetFileIdentity(destinationHandle)))
                {
                    throw new IOException("The renamed directory identity is invalid.");
                }
            }
            catch
            {
                if (renameCompleted && sourceHandle != null)
                {
                    try
                    {
                        RenameHandle(sourceHandle, source);
                    }
                    catch
                    {
                    }
                }
                throw;
            }
            finally
            {
                if (destinationHandle != null)
                {
                    destinationHandle.Dispose();
                }
                if (sourceHandle != null)
                {
                    sourceHandle.Dispose();
                }
                if (destinationParentHandle != null)
                {
                    destinationParentHandle.Dispose();
                }
                if (sourceParentHandle != null)
                {
                    sourceParentHandle.Dispose();
                }
                ancestorLocks.Dispose();
            }
        }

        public static string GetPathIdentity(string path)
        {
            string target = NormalizePath(path);
            PathLockSession ancestorLocks = new PathLockSession();
            SafeFileHandle targetHandle = null;
            try
            {
                AddAncestorIdentityLocks(target, ancestorLocks);
                bool targetIsDirectory;
                targetHandle = OpenVerifiedHandle(
                    target,
                    FileReadDataOrListDirectory,
                    FileShareRead | FileShareWrite | FileShareDelete,
                    out targetIsDirectory);
                return FormatFileIdentity(GetFileIdentity(targetHandle));
            }
            finally
            {
                if (targetHandle != null)
                {
                    targetHandle.Dispose();
                }
                ancestorLocks.Dispose();
            }
        }

        private static FileIdInformation GetFileIdentity(
            SafeFileHandle handle)
        {
            FileIdInformation information;
            if (!GetFileIdInformationByHandleEx(
                    handle,
                    FileIdInfo,
                    out information,
                    (uint)Marshal.SizeOf(typeof(FileIdInformation))))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error());
            }
            return information;
        }

        private static bool SameFileIdentity(
            FileIdInformation left,
            FileIdInformation right)
        {
            return left.VolumeSerialNumber == right.VolumeSerialNumber &&
                left.FileIdPart0 == right.FileIdPart0 &&
                left.FileIdPart1 == right.FileIdPart1;
        }

        private static string FormatFileIdentity(FileIdInformation identity)
        {
            return "v1:" +
                identity.VolumeSerialNumber.ToString(
                    "x16",
                    CultureInfo.InvariantCulture) +
                ":" +
                identity.FileIdPart0.ToString(
                    "x16",
                    CultureInfo.InvariantCulture) +
                identity.FileIdPart1.ToString(
                    "x16",
                    CultureInfo.InvariantCulture);
        }

        private static void AddAncestorIdentityLocks(
            string target,
            PathLockSession session)
        {
            List<string> ancestors = new List<string>();
            DirectoryInfo ancestor = Directory.GetParent(target);
            while (ancestor != null)
            {
                ancestors.Add(ancestor.FullName);
                ancestor = ancestor.Parent;
            }
            ancestors.Reverse();

            foreach (string path in ancestors)
            {
                bool isDirectory;
                OpenVerifiedPath(
                    path,
                    FileReadDataOrListDirectory,
                    FileShareRead | FileShareWrite,
                    session,
                    out isDirectory);
                if (!isDirectory)
                {
                    throw new IOException("A path ancestor is not a directory.");
                }
            }
        }

        private static void OpenVerifiedPath(
            string path,
            uint desiredAccess,
            uint shareMode,
            PathLockSession session,
            out bool isDirectory)
        {
            SafeFileHandle handle = OpenVerifiedHandle(
                path,
                desiredAccess,
                shareMode,
                out isDirectory);
            try
            {
                session.Add(handle);
                handle = null;
            }
            finally
            {
                if (handle != null)
                {
                    handle.Dispose();
                }
            }
        }

        private static SafeFileHandle OpenVerifiedHandle(
            string path,
            uint desiredAccess,
            uint shareMode,
            out bool isDirectory)
        {
            string normalized = NormalizePath(path);
            SafeFileHandle handle = CreateFileW(
                ToExtendedPath(normalized),
                desiredAccess,
                shareMode,
                IntPtr.Zero,
                OpenExisting,
                FileFlagBackupSemantics | FileFlagOpenReparsePoint,
                IntPtr.Zero);
            if (handle.IsInvalid)
            {
                int error = Marshal.GetLastWin32Error();
                handle.Dispose();
                throw new Win32Exception(error);
            }

            try
            {
                FileAttributeTagInformation information;
                if (!GetFileInformationByHandleEx(
                        handle,
                        FileAttributeTagInfo,
                        out information,
                        (uint)Marshal.SizeOf(typeof(FileAttributeTagInformation))))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                if ((information.FileAttributes & FileAttributeReparsePoint) != 0)
                {
                    throw new IOException("Reparse points are not trusted.");
                }

                string openedPath = GetFinalPath(handle);
                if (!String.Equals(
                        NormalizePathForComparison(openedPath),
                        NormalizePathForComparison(normalized),
                        StringComparison.OrdinalIgnoreCase))
                {
                    throw new IOException("The opened path identity changed.");
                }

                isDirectory =
                    (information.FileAttributes & FileAttributeDirectory) != 0;
                return handle;
            }
            catch
            {
                handle.Dispose();
                throw;
            }
        }

        private static void RenameHandle(
            SafeFileHandle sourceHandle,
            string destinationPath)
        {
            byte[] destinationBytes = Encoding.Unicode.GetBytes(destinationPath);
            int rootDirectoryOffset = IntPtr.Size == 8 ? 8 : 4;
            int fileNameLengthOffset = rootDirectoryOffset + IntPtr.Size;
            int fileNameOffset = fileNameLengthOffset + sizeof(uint);
            int bufferSize = checked(fileNameOffset + destinationBytes.Length + 2);
            IntPtr buffer = Marshal.AllocHGlobal(bufferSize);
            try
            {
                byte[] empty = new byte[bufferSize];
                Marshal.Copy(empty, 0, buffer, empty.Length);
                Marshal.WriteInt32(buffer, 0, 0);
                Marshal.WriteIntPtr(buffer, rootDirectoryOffset, IntPtr.Zero);
                Marshal.WriteInt32(
                    buffer,
                    fileNameLengthOffset,
                    destinationBytes.Length);
                Marshal.Copy(
                    destinationBytes,
                    0,
                    IntPtr.Add(buffer, fileNameOffset),
                    destinationBytes.Length);
                if (!SetFileInformationByHandle(
                        sourceHandle,
                        FileRenameInfo,
                        buffer,
                        (uint)bufferSize))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
            }
            finally
            {
                Marshal.FreeHGlobal(buffer);
            }
        }

        private static string GetFinalPath(SafeFileHandle handle)
        {
            int capacity = 512;
            while (capacity <= MaxPathCharacters + 1)
            {
                StringBuilder result = new StringBuilder(capacity);
                uint length = GetFinalPathNameByHandleW(
                    handle,
                    result,
                    (uint)result.Capacity,
                    0);
                if (length == 0)
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                if (length < result.Capacity)
                {
                    return result.ToString();
                }
                if (length > MaxPathCharacters)
                {
                    break;
                }
                capacity = checked((int)length + 1);
            }
            throw new PathTooLongException();
        }

        private static string NormalizePath(string path)
        {
            if (String.IsNullOrWhiteSpace(path) || path.IndexOf('\0') >= 0)
            {
                throw new ArgumentException("The path is invalid.", "path");
            }
            string normalized = Path.GetFullPath(path);
            string root = Path.GetPathRoot(normalized);
            if (!String.Equals(normalized, root, StringComparison.OrdinalIgnoreCase))
            {
                normalized = normalized.TrimEnd(
                    Path.DirectorySeparatorChar,
                    Path.AltDirectorySeparatorChar);
            }
            if (normalized.Length > MaxPathCharacters)
            {
                throw new PathTooLongException();
            }
            return normalized;
        }

        private static string NormalizePathForComparison(string path)
        {
            string normalized = path;
            if (normalized.StartsWith(
                    @"\\?\UNC\",
                    StringComparison.OrdinalIgnoreCase))
            {
                normalized = @"\\" + normalized.Substring(8);
            }
            else if (normalized.StartsWith(
                         @"\\?\",
                         StringComparison.OrdinalIgnoreCase))
            {
                normalized = normalized.Substring(4);
            }
            normalized = normalized.Replace(
                Path.AltDirectorySeparatorChar,
                Path.DirectorySeparatorChar);
            string root = Path.GetPathRoot(normalized);
            if (!String.Equals(normalized, root, StringComparison.OrdinalIgnoreCase))
            {
                normalized = normalized.TrimEnd(Path.DirectorySeparatorChar);
            }
            return normalized;
        }

        private static string ToExtendedPath(string path)
        {
            if (path.StartsWith(@"\\?\", StringComparison.Ordinal))
            {
                return path;
            }
            if (path.StartsWith(@"\\", StringComparison.Ordinal))
            {
                return @"\\?\UNC\" + path.Substring(2);
            }
            return @"\\?\" + path;
        }
    }
}
'@
}

function Resolve-RestorePlaneAbsolutePath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [switch] $AllowMissing,
        [switch] $AllowVolumeRoot
    )

    if ([string]::IsNullOrWhiteSpace($Path) -or $Path.IndexOf([char]0) -ge 0) {
        throw "INSTALL_PATH_INVALID"
    }
    $resolved = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($resolved)
    if ($resolved -eq $root -and -not $AllowVolumeRoot) {
        throw "INSTALL_PATH_TOO_BROAD"
    }
    if (-not $AllowMissing -and -not (Test-Path -LiteralPath $resolved)) {
        throw "INSTALL_PATH_NOT_FOUND"
    }
    return $resolved
}

function Enter-RestorePlanePathMutationLock {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [switch] $AllowVolumeRoot
    )

    try {
        $resolved = Resolve-RestorePlaneAbsolutePath `
            -Path $Path `
            -AllowVolumeRoot:$AllowVolumeRoot
        return [RestorePlane.Installation.NativePathLock]::AcquirePathMutationLock(
            $resolved
        )
    }
    catch {
        throw "INSTALL_TREE_LOCK_FAILED"
    }
}

function Enter-RestorePlaneInstallTreeLock {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)] [string] $InstallRoot)

    try {
        $resolved = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot
        return [RestorePlane.Installation.NativePathLock]::AcquireTree(
            $resolved,
            32768
        )
    }
    catch {
        throw "INSTALL_TREE_LOCK_FAILED"
    }
}

function Move-RestorePlaneDirectoryByHandleAtomically {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $SourcePath,
        [Parameter(Mandatory = $true)] [string] $DestinationPath
    )

    try {
        $source = Resolve-RestorePlaneAbsolutePath -Path $SourcePath
        $destination = Resolve-RestorePlaneAbsolutePath `
            -Path $DestinationPath `
            -AllowMissing
        [RestorePlane.Installation.NativePathLock]::MoveDirectoryByHandle(
            $source,
            $destination
        )
    }
    catch {
        throw "INSTALL_DIRECTORY_MOVE_FAILED"
    }
}

function Get-RestorePlanePathIdentity {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)] [string] $Path)

    try {
        $resolved = Resolve-RestorePlaneAbsolutePath -Path $Path
        return [RestorePlane.Installation.NativePathLock]::GetPathIdentity(
            $resolved
        )
    }
    catch {
        throw "INSTALL_PATH_IDENTITY_FAILED"
    }
}

function Assert-RestorePlaneSeparatedRoots {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $DataRoot
    )

    $install = [IO.Path]::GetFullPath($InstallRoot).TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    )
    $data = [IO.Path]::GetFullPath($DataRoot).TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    )
    $separator = [IO.Path]::DirectorySeparatorChar
    if ($install.Equals($data, [StringComparison]::OrdinalIgnoreCase) -or
        $install.StartsWith(
            $data + $separator,
            [StringComparison]::OrdinalIgnoreCase
        ) -or
        $data.StartsWith(
            $install + $separator,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw "INSTALL_DATA_ROOT_OVERLAP"
    }
}

function Assert-RestorePlaneChildPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [Parameter(Mandatory = $true)] [string] $Candidate
    )

    $resolvedRoot = [IO.Path]::GetFullPath($Root).TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    )
    $resolvedCandidate = [IO.Path]::GetFullPath($Candidate)
    $prefix = $resolvedRoot + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedCandidate.StartsWith(
            $prefix,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw "INSTALL_PATH_ESCAPE"
    }
    return $resolvedCandidate
}

function Get-RestorePlaneByteSha256 {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)] [byte[]] $Bytes)

    $hasher = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString(
                $hasher.ComputeHash($Bytes)
            )).Replace("-", "")
    }
    finally {
        $hasher.Dispose()
    }
}

function Read-RestorePlanePackageManifest {
    [CmdletBinding(DefaultParameterSetName = "Path")]
    param(
        [Parameter(Mandatory = $true, ParameterSetName = "Path")]
        [string] $ManifestPath,
        [Parameter(Mandatory = $true, ParameterSetName = "Bytes")]
        [byte[]] $ManifestBytes
    )

    if ($PSCmdlet.ParameterSetName -eq "Path") {
        $resolved = Resolve-RestorePlaneAbsolutePath -Path $ManifestPath
        $metadata = Get-Item -LiteralPath $resolved -Force
        if (-not $metadata.PSIsContainer -and
            $metadata.Length -gt 0 -and
            $metadata.Length -le 1MB -and
            -not ($metadata.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            $manifest = Get-Content -LiteralPath $resolved -Raw -Encoding UTF8 |
                ConvertFrom-Json
        }
        else {
            throw "PACKAGE_MANIFEST_INVALID"
        }
    }
    else {
        if ($ManifestBytes.Length -le 0 -or $ManifestBytes.Length -gt 1MB) {
            throw "PACKAGE_MANIFEST_INVALID"
        }
        try {
            $encoding = [Text.UTF8Encoding]::new($false, $true)
            $json = $encoding.GetString($ManifestBytes)
            if ($json.Length -gt 0 -and $json[0] -eq [char]0xFEFF) {
                $json = $json.Substring(1)
            }
            $manifest = $json | ConvertFrom-Json
        }
        catch {
            throw "PACKAGE_MANIFEST_INVALID"
        }
    }
    if ($manifest.schemaVersion -ne 1 -or
        $manifest.channel -notin @("development", "production") -or
        [string]::IsNullOrWhiteSpace($manifest.version) -or
        [string]::IsNullOrWhiteSpace($manifest.buildId) -or
        [string]::IsNullOrWhiteSpace($manifest.revision) -or
        ([string] $manifest.buildId) -notmatch "^[0-9a-fA-F]{64}$" -or
        ([string] $manifest.revision) -notmatch "^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$" -or
        $null -eq $manifest.files -or
        @($manifest.files).Count -eq 0 -or
        @($manifest.files).Count -gt 8192) {
        throw "PACKAGE_MANIFEST_INVALID"
    }
    $version = $null
    $minimumReader = $null
    if (-not [Version]::TryParse($manifest.version, [ref] $version) -or
        -not [Version]::TryParse($manifest.minimumReaderVersion, [ref] $minimumReader) -or
        $version.ToString(3) -ne $manifest.version -or
        $minimumReader.ToString(3) -ne $manifest.minimumReaderVersion -or
        $minimumReader -gt $version) {
        throw "PACKAGE_VERSION_INVALID"
    }
    $seen = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    foreach ($file in @($manifest.files)) {
        $relative = [string] $file.path
        $signerRoleProperty = $file.PSObject.Properties["signerRole"]
        $signerThumbprintProperty = $file.PSObject.Properties["signerThumbprint"]
        if ([string]::IsNullOrWhiteSpace($relative) -or
            [IO.Path]::IsPathRooted($relative) -or
            $relative.Contains("..") -or
            $relative.IndexOf([char]0) -ge 0 -or
            $file.sha256 -notmatch "^[0-9a-fA-F]{64}$" -or
            $null -eq $file.PSObject.Properties["sizeBytes"] -or
            [long] $file.sizeBytes -le 0 -or
            [long] $file.sizeBytes -gt 2GB -or
            $null -eq $signerRoleProperty -or
            $null -eq $signerThumbprintProperty -or
            ([string] $file.signerRole) -notin @(
                "release", "vendor", "hash_only"
            ) -or
            ([string] $file.signerThumbprint) -notmatch (
                "^(?:|[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$"
            ) -or
            -not ($file.authenticodeRequired -is [bool]) -or
            -not $seen.Add($relative.Replace("\", "/"))) {
            throw "PACKAGE_FILE_ENTRY_INVALID"
        }
        $extension = [IO.Path]::GetExtension($relative).ToLowerInvariant()
        $isExecutable = $extension -in @(".exe", ".dll", ".msi")
        $isNativeTool = $relative.Replace("\", "/").StartsWith(
            "tools/",
            [StringComparison]::Ordinal
        )
        if ($manifest.channel -eq "production" -and $isExecutable) {
            if ($isNativeTool -and $extension -eq ".dll") {
                if ($file.authenticodeRequired -eq $true -and
                    [string] $file.signerRole -ne "vendor") {
                    throw "PACKAGE_SIGNATURE_POLICY_INVALID"
                }
            }
            elseif ($file.authenticodeRequired -ne $true -or
                ([string] $file.signerRole) -notin @("release", "vendor")) {
                throw "PACKAGE_SIGNATURE_POLICY_INVALID"
            }
        }
        if ($file.authenticodeRequired -eq $true -and
            [string] $file.signerRole -eq "vendor" -and
            ([string] $file.signerThumbprint) -notmatch (
                "^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$"
            )) {
            throw "PACKAGE_SIGNATURE_POLICY_INVALID"
        }
        if (($file.authenticodeRequired -eq $true) -ne
            ([string] $file.signerRole -in @("release", "vendor"))) {
            throw "PACKAGE_SIGNATURE_POLICY_INVALID"
        }
    }
    if ($manifest.channel -eq "production" -and
        -not $seen.Contains("provider-certifications.json")) {
        throw "PACKAGE_PROVIDER_CERTIFICATIONS_REQUIRED"
    }
    $nativeToolsProperty = $manifest.PSObject.Properties["nativeTools"]
    if ($manifest.channel -eq "production" -and $null -eq $nativeToolsProperty) {
        throw "PACKAGE_NATIVE_TOOLS_REQUIRED"
    }
    if ($null -ne $nativeToolsProperty) {
        $nativeTools = $manifest.nativeTools
        if (([string] $nativeTools.auditManifestPath).Replace("\", "/") -cne
            "tools/native-tools-audit.json" -or
            ([string] $nativeTools.auditManifestSha256) -notmatch "^[0-9a-fA-F]{64}$" -or
            [int] $nativeTools.fileCount -le 0 -or
            [int] $nativeTools.fileCount -gt 4096) {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_INVALID"
        }
    }
    return $manifest
}

function Read-RestorePlaneTrustedPackageManifest {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $ManifestPath,
        [string] $ExpectedSha256,
        [switch] $AllowMissingExpectedDevelopmentFixture
    )

    $resolved = Resolve-RestorePlaneAbsolutePath -Path $ManifestPath
    $metadata = Get-Item -LiteralPath $resolved -Force
    if ($metadata.PSIsContainer -or
        $metadata.Length -le 0 -or
        $metadata.Length -gt 1MB -or
        $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "PACKAGE_MANIFEST_INVALID"
    }
    $hasExpected = -not [string]::IsNullOrWhiteSpace($ExpectedSha256)
    if ($hasExpected) {
        if ($ExpectedSha256 -notmatch "^[0-9a-fA-F]{64}$") {
            throw "PACKAGE_MANIFEST_DIGEST_REQUIRED"
        }
        $expected = $ExpectedSha256.ToUpperInvariant()
    }
    elseif (-not $AllowMissingExpectedDevelopmentFixture) {
        throw "PACKAGE_MANIFEST_DIGEST_REQUIRED"
    }

    $stream = $null
    try {
        $stream = [IO.FileStream]::new(
            $resolved,
            [IO.FileMode]::Open,
            [IO.FileAccess]::Read,
            [IO.FileShare]::Read,
            4096,
            [IO.FileOptions]::SequentialScan
        )
        if ($stream.Length -le 0 -or $stream.Length -gt 1MB) {
            throw "PACKAGE_MANIFEST_INVALID"
        }
        $bytes = [byte[]]::new([int] $stream.Length)
        $offset = 0
        while ($offset -lt $bytes.Length) {
            $read = $stream.Read($bytes, $offset, $bytes.Length - $offset)
            if ($read -le 0) {
                throw "PACKAGE_MANIFEST_INVALID"
            }
            $offset += $read
        }
        if ($stream.ReadByte() -ne -1) {
            throw "PACKAGE_MANIFEST_INVALID"
        }
    }
    finally {
        if ($null -ne $stream) {
            $stream.Dispose()
        }
    }

    $actualSha256 = Get-RestorePlaneByteSha256 -Bytes $bytes
    if ($hasExpected -and $actualSha256 -cne $expected) {
        throw "PACKAGE_MANIFEST_DIGEST_MISMATCH"
    }
    if (-not $hasExpected) {
        try {
            $fixtureJson = [Text.UTF8Encoding]::new($false, $true).GetString(
                $bytes
            )
            $fixtureChannel = [string] (
                $fixtureJson | ConvertFrom-Json
            ).channel
        }
        catch {
            throw "PACKAGE_MANIFEST_DIGEST_REQUIRED"
        }
        if ($fixtureChannel -ne "development") {
            throw "PACKAGE_MANIFEST_DIGEST_REQUIRED"
        }
        $expected = $actualSha256
    }
    $manifest = Read-RestorePlanePackageManifest -ManifestBytes $bytes
    return [PSCustomObject] @{
        manifest = $manifest
        sha256 = $expected
        bytes = $bytes
    }
}

function Test-RestorePlanePackage {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $PackageRoot,
        [Parameter(Mandatory = $true)] [object] $Manifest,
        [string] $ExpectedSignerThumbprint,
        [string] $ExpectedNativeToolAuditSha256,
        [switch] $AllowUnsignedDevelopmentPackage
    )

    $resolvedRoot = Resolve-RestorePlaneAbsolutePath -Path $PackageRoot
    if ($Manifest.channel -eq "development" -and
        -not $AllowUnsignedDevelopmentPackage) {
        throw "PACKAGE_DEVELOPMENT_CHANNEL_NOT_ALLOWED"
    }
    if ($Manifest.channel -eq "production" -and
        [string]::IsNullOrWhiteSpace($ExpectedSignerThumbprint)) {
        throw "PACKAGE_SIGNER_REQUIRED"
    }
    $normalizedThumbprint = if ([string]::IsNullOrWhiteSpace($ExpectedSignerThumbprint)) {
        ""
    }
    else {
        $ExpectedSignerThumbprint.Replace(" ", "").ToUpperInvariant()
    }
    $nativeToolsProperty = $Manifest.PSObject.Properties["nativeTools"]
    $auditByPackagePath = @{}
    if ($null -ne $nativeToolsProperty) {
        if ($ExpectedNativeToolAuditSha256 -notmatch "^[0-9a-fA-F]{64}$") {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_DIGEST_REQUIRED"
        }
        $expectedAuditHash = $ExpectedNativeToolAuditSha256.ToLowerInvariant()
        if (([string] $Manifest.nativeTools.auditManifestSha256).ToLowerInvariant() -ne
            $expectedAuditHash) {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_DIGEST_MISMATCH"
        }
        $auditPath = Assert-RestorePlaneChildPath -Root $resolvedRoot -Candidate (
            Join-Path $resolvedRoot ([string] $Manifest.nativeTools.auditManifestPath)
        )
        if (-not (Test-Path -LiteralPath $auditPath -PathType Leaf)) {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_DIGEST_MISMATCH"
        }
        $actualAuditHash = (
            Get-FileHash -LiteralPath $auditPath -Algorithm SHA256
        ).Hash.ToLowerInvariant()
        if ($actualAuditHash -ne $expectedAuditHash) {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_DIGEST_MISMATCH"
        }
        try {
            $audit = Get-Content -LiteralPath $auditPath -Raw -Encoding UTF8 |
                ConvertFrom-Json
        }
        catch {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_INVALID"
        }
        if ($audit.schemaVersion -ne 1 -or
            @($audit.files).Count -ne [int] $Manifest.nativeTools.fileCount) {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_INVALID"
        }
        foreach ($tool in @($audit.files)) {
            $packagePath = "tools/" + ([string] $tool.path).Replace("\", "/")
            if ($auditByPackagePath.ContainsKey($packagePath)) {
                throw "PACKAGE_NATIVE_TOOL_AUDIT_INVALID"
            }
            $auditByPackagePath[$packagePath] = $tool
        }
    }
    elseif ($Manifest.channel -eq "production") {
        throw "PACKAGE_NATIVE_TOOLS_REQUIRED"
    }

    $manifestedPaths = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    foreach ($entry in @($Manifest.files)) {
        $null = $manifestedPaths.Add(([string] $entry.path).Replace("\", "/"))
        $candidate = Assert-RestorePlaneChildPath -Root $resolvedRoot -Candidate (
            Join-Path $resolvedRoot ([string] $entry.path)
        )
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "PACKAGE_FILE_MISSING"
        }
        $metadata = Get-Item -LiteralPath $candidate -Force
        if ($metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "PACKAGE_REPARSE_POINT_BLOCKED"
        }
        if ($metadata.Length -ne [long] $entry.sizeBytes) {
            throw "PACKAGE_SIZE_MISMATCH"
        }
        $actualHash = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash
        if ($actualHash -ne ([string] $entry.sha256).ToUpperInvariant()) {
            throw "PACKAGE_HASH_MISMATCH"
        }
        $relative = ([string] $entry.path).Replace("\", "/")
        if ($relative.StartsWith("tools/", [StringComparison]::Ordinal) -and
            $relative -ne "tools/native-tools-audit.json") {
            if (-not $auditByPackagePath.ContainsKey($relative)) {
                throw "PACKAGE_NATIVE_TOOL_TREE_INCOMPLETE"
            }
            $tool = $auditByPackagePath[$relative]
            $expectedRole = if ($tool.authenticodeRequired -eq $true) {
                "vendor"
            }
            else {
                "hash_only"
            }
            if (([string] $entry.sha256).ToLowerInvariant() -ne
                    ([string] $tool.sha256).ToLowerInvariant() -or
                [string] $entry.signerRole -ne $expectedRole -or
                ([string] $entry.signerThumbprint).Replace(" ", "").ToLowerInvariant() -ne
                    ([string] $tool.signerThumbprint).Replace(" ", "").ToLowerInvariant() -or
                [string] $entry.publisher -ne [string] $tool.publisher) {
                throw "PACKAGE_NATIVE_TOOL_POLICY_MISMATCH"
            }
        }
        if ($entry.authenticodeRequired -eq $true) {
            $signature = Get-AuthenticodeSignature -LiteralPath $candidate
            $entrySigner = if ([string] $entry.signerRole -eq "vendor") {
                ([string] $entry.signerThumbprint).Replace(" ", "").ToUpperInvariant()
            }
            else {
                $normalizedThumbprint
            }
            if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid -or
                $null -eq $signature.SignerCertificate -or
                $signature.SignerCertificate.Thumbprint.Replace(
                    " ",
                    ""
                ).ToUpperInvariant() -ne $entrySigner) {
                throw "PACKAGE_AUTHENTICODE_INVALID"
            }
            if (-not [string]::IsNullOrWhiteSpace([string] $entry.publisher) -and
                -not $signature.SignerCertificate.Subject.Contains(
                    [string] $entry.publisher,
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                throw "PACKAGE_PUBLISHER_MISMATCH"
            }
        }
    }
    foreach ($nativePath in $auditByPackagePath.Keys) {
        if (-not $manifestedPaths.Contains($nativePath)) {
            throw "PACKAGE_NATIVE_TOOL_TREE_INCOMPLETE"
        }
    }
    $rootManifestPath = [IO.Path]::GetFullPath(
        (Join-Path $resolvedRoot "package-manifest.json")
    )
    $packageItems = @(
        Get-ChildItem -LiteralPath $resolvedRoot -Recurse -Force
    )
    if (@($packageItems | Where-Object {
                $_.Attributes -band [IO.FileAttributes]::ReparsePoint
            }).Count -ne 0) {
        throw "PACKAGE_REPARSE_POINT_BLOCKED"
    }
    $actualPayload = @(
        $packageItems |
            Where-Object {
                -not $_.PSIsContainer -and
                $_.FullName -ne $rootManifestPath
            }
    )
    foreach ($file in $actualPayload) {
        if ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "PACKAGE_REPARSE_POINT_BLOCKED"
        }
        $relative = $file.FullName.Substring($resolvedRoot.Length).
            TrimStart("\", "/").Replace("\", "/")
        if (-not $manifestedPaths.Contains($relative)) {
            throw "PACKAGE_UNMANIFESTED_FILE"
        }
    }
    if ($actualPayload.Count -ne @($Manifest.files).Count) {
        throw "PACKAGE_FILE_SET_INCOMPLETE"
    }
}

function Assert-RestorePlaneAdministrator {
    [CmdletBinding()]
    param()

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole(
            [Security.Principal.WindowsBuiltInRole]::Administrator
        )) {
        throw "INSTALL_ADMINISTRATOR_REQUIRED"
    }
}

function Invoke-RestorePlaneNative {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $FilePath,
        [Parameter(Mandatory = $true)] [string[]] $Arguments
    )

    $lines = @(& $FilePath @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        $tail = @($lines | Select-Object -Last 10) -join "`n"
        throw "INSTALL_NATIVE_COMMAND_FAILED: $tail"
    }
    return @($lines)
}

function Get-RestorePlaneServiceSid {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)] [string] $ServiceName)

    $lines = Invoke-RestorePlaneNative -FilePath "sc.exe" -Arguments @(
        "showsid",
        $ServiceName
    )
    $match = [regex]::Match(
        ($lines -join "`n"),
        "S-1-5-80-(?:\d+-){4}\d+",
        [Text.RegularExpressions.RegexOptions]::CultureInvariant
    )
    if (-not $match.Success) {
        throw "INSTALL_SERVICE_SID_UNAVAILABLE"
    }
    return $match.Value
}

function Set-RestorePlaneProtectedAcl {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [string] $ServiceSid,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [ValidateSet("full", "read-execute")] [string] $ServiceAccess = "full",
        [switch] $Recurse
    )

    $resolved = Resolve-RestorePlaneAbsolutePath -Path $Path
    try {
        $owner = [Security.Principal.SecurityIdentifier]::new($OwnerSid)
        $system = [Security.Principal.SecurityIdentifier]::new("S-1-5-18")
        $service = if ([string]::IsNullOrWhiteSpace($ServiceSid)) {
            $null
        }
        else {
            [Security.Principal.SecurityIdentifier]::new($ServiceSid)
        }
    }
    catch {
        throw "INSTALL_ACL_IDENTITY_INVALID"
    }
    if ($owner.Value -eq $system.Value -or
        ($null -ne $service -and
            ($service.Value -in @($system.Value, $owner.Value) -or
                $service.Value -notmatch "^S-1-5-80-(?:\d+-){4}\d+$"))) {
        throw "INSTALL_ACL_IDENTITY_INVALID"
    }

    function Set-ExactAcl {
        param([Parameter(Mandatory = $true)] [IO.FileSystemInfo] $Metadata)

        if ($Metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "INSTALL_TREE_REPARSE_POINT_BLOCKED"
        }
        $isDirectory = $Metadata.PSIsContainer
        $acl = if ($isDirectory) {
            [Security.AccessControl.DirectorySecurity]::new()
        }
        else {
            [Security.AccessControl.FileSecurity]::new()
        }
        $acl.SetAccessRuleProtection($true, $false)
        $acl.SetOwner($owner)
        $inheritance = if ($isDirectory) {
            [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
                [Security.AccessControl.InheritanceFlags]::ObjectInherit
        }
        else {
            [Security.AccessControl.InheritanceFlags]::None
        }
        $fullRights = [Security.AccessControl.FileSystemRights]::FullControl
        $serviceRights = if ($ServiceAccess -eq "full") {
            $fullRights
        }
        else {
            [Security.AccessControl.FileSystemRights]::ReadAndExecute
        }
        $rules = [Collections.Generic.List[object]]::new()
        $rules.Add(@($system, $fullRights))
        $rules.Add(@($owner, $fullRights))
        if ($null -ne $service) {
            $rules.Add(@($service, $serviceRights))
        }
        foreach ($rule in $rules) {
            $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
                    [Security.Principal.IdentityReference] $rule[0],
                    [Security.AccessControl.FileSystemRights] $rule[1],
                    $inheritance,
                    [Security.AccessControl.PropagationFlags]::None,
                    [Security.AccessControl.AccessControlType]::Allow
                ))
        }
        $mutationLock = Enter-RestorePlanePathMutationLock -Path $Metadata.FullName
        try {
            Set-Acl -LiteralPath $Metadata.FullName -AclObject $acl
        }
        finally {
            $mutationLock.Dispose()
        }
    }

    $root = Get-Item -LiteralPath $resolved -Force
    Set-ExactAcl -Metadata $root
    if (-not $Recurse -or -not $root.PSIsContainer) {
        return
    }

    $visited = 1
    $directories = [Collections.Generic.Queue[IO.DirectoryInfo]]::new()
    $directories.Enqueue([IO.DirectoryInfo] $root)
    while ($directories.Count -gt 0) {
        $directory = $directories.Dequeue()
        foreach ($item in @(Get-ChildItem -LiteralPath $directory.FullName -Force)) {
            $visited += 1
            if ($visited -gt 32768) {
                throw "INSTALL_ACL_TARGET_LIMIT_EXCEEDED"
            }
            Set-ExactAcl -Metadata $item
            if ($item.PSIsContainer) {
                $directories.Enqueue([IO.DirectoryInfo] $item)
            }
        }
    }
}

function Assert-RestorePlaneProtectedAcl {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [string] $ServiceSid,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [ValidateSet("full", "read-execute")] [string] $ServiceAccess = "full",
        [switch] $Recurse
    )

    try {
        $resolved = Resolve-RestorePlaneAbsolutePath -Path $Path
        $owner = [Security.Principal.SecurityIdentifier]::new($OwnerSid)
        $system = [Security.Principal.SecurityIdentifier]::new("S-1-5-18")
        $service = if ([string]::IsNullOrWhiteSpace($ServiceSid)) {
            $null
        }
        else {
            [Security.Principal.SecurityIdentifier]::new($ServiceSid)
        }
        if ($owner.Value -eq $system.Value -or
            ($null -ne $service -and
                ($service.Value -in @($system.Value, $owner.Value) -or
                    $service.Value -notmatch "^S-1-5-80-(?:\d+-){4}\d+$"))) {
            throw "INSTALL_ACL_INVALID"
        }

        $expectedRights = @{
            $system.Value = [long] [Security.AccessControl.FileSystemRights]::FullControl
        }
        $expectedRights[$owner.Value] = [long] (
            [Security.AccessControl.FileSystemRights]::FullControl
        )
        if ($null -ne $service) {
            $expectedRights[$service.Value] = if ($ServiceAccess -eq "full") {
                [long] [Security.AccessControl.FileSystemRights]::FullControl
            }
            else {
                [long] (
                    [Security.AccessControl.FileSystemRights]::ReadAndExecute -bor
                    [Security.AccessControl.FileSystemRights]::Synchronize
                )
            }
        }

        $targets = [Collections.Generic.List[IO.FileSystemInfo]]::new()
        $root = Get-Item -LiteralPath $resolved -Force
        $targets.Add($root)
        if ($Recurse -and $root.PSIsContainer) {
            $directories = [Collections.Generic.Queue[IO.DirectoryInfo]]::new()
            $directories.Enqueue([IO.DirectoryInfo] $root)
            while ($directories.Count -gt 0) {
                $directory = $directories.Dequeue()
                foreach ($item in @(Get-ChildItem -LiteralPath $directory.FullName -Force)) {
                    if ($targets.Count -ge 32768) {
                        throw "INSTALL_ACL_INVALID"
                    }
                    $targets.Add($item)
                    if ($item.PSIsContainer) {
                        $directories.Enqueue([IO.DirectoryInfo] $item)
                    }
                }
            }
        }

        foreach ($target in $targets) {
            if ($target.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "INSTALL_ACL_INVALID"
            }
            $acl = Get-Acl -LiteralPath $target.FullName
            $actualOwner = $acl.GetOwner(
                [Security.Principal.SecurityIdentifier]
            ).Value
            if (-not $acl.AreAccessRulesProtected -or
                $actualOwner -ne $owner.Value) {
                throw "INSTALL_ACL_INVALID"
            }
            $expectedInheritance = if ($target.PSIsContainer) {
                [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
                    [Security.AccessControl.InheritanceFlags]::ObjectInherit
            }
            else {
                [Security.AccessControl.InheritanceFlags]::None
            }
            $rules = @($acl.GetAccessRules(
                    $true,
                    $true,
                    [Security.Principal.SecurityIdentifier]
                ))
            if ($rules.Count -ne $expectedRights.Count) {
                throw "INSTALL_ACL_INVALID"
            }
            $seen = [Collections.Generic.HashSet[string]]::new(
                [StringComparer]::Ordinal
            )
            foreach ($rule in $rules) {
                $sid = $rule.IdentityReference.Value
                if (-not $expectedRights.ContainsKey($sid) -or
                    -not $seen.Add($sid) -or
                    $rule.AccessControlType -ne
                        [Security.AccessControl.AccessControlType]::Allow -or
                    $rule.IsInherited -or
                    [long] $rule.FileSystemRights -ne $expectedRights[$sid] -or
                    $rule.InheritanceFlags -ne $expectedInheritance -or
                    $rule.PropagationFlags -ne
                        [Security.AccessControl.PropagationFlags]::None) {
                    throw "INSTALL_ACL_INVALID"
                }
            }
        }
    }
    catch {
        throw "INSTALL_ACL_INVALID"
    }
}

function Resolve-RestorePlaneOwnerSid {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $DataRoot,
        [string] $RequestedOwnerSid,
        [switch] $RequirePersisted
    )

    $data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot -AllowMissing
    $identityPath = Assert-RestorePlaneChildPath -Root $data -Candidate (
        Join-Path $data "service-identity.json"
    )
    $requested = if ([string]::IsNullOrWhiteSpace($RequestedOwnerSid)) {
        $null
    }
    else {
        try {
            [Security.Principal.SecurityIdentifier]::new(
                $RequestedOwnerSid
            ).Value
        }
        catch {
            throw "INSTALL_ACL_IDENTITY_INVALID"
        }
    }
    if (-not (Test-Path -LiteralPath $identityPath)) {
        if ($RequirePersisted) {
            throw "INSTALL_IDENTITY_REQUIRED"
        }
        if ($null -ne $requested) {
            if ($requested -eq "S-1-5-18") {
                throw "INSTALL_ACL_IDENTITY_INVALID"
            }
            $currentSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
            if ($currentSid -notin @("S-1-5-18", $requested)) {
                throw "INSTALL_OWNER_CONTEXT_REQUIRED"
            }
            return $requested
        }
        $currentSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        if ($currentSid -eq "S-1-5-18") {
            throw "INSTALL_OWNER_SID_REQUIRED"
        }
        return $currentSid
    }

    try {
        $metadata = Get-Item -LiteralPath $identityPath -Force
        if ($metadata.PSIsContainer -or
            $metadata.Length -le 0 -or
            $metadata.Length -gt 64KB -or
            $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "INSTALL_IDENTITY_INVALID"
        }
        $acl = Get-Acl -LiteralPath $identityPath
        $aclOwner = $acl.GetOwner(
            [Security.Principal.SecurityIdentifier]
        ).Value
        $currentSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        if ($currentSid -notin @("S-1-5-18", $aclOwner)) {
            throw "INSTALL_OWNER_CONTEXT_REQUIRED"
        }
        $identity = Get-Content -LiteralPath $identityPath -Raw -Encoding UTF8 |
            ConvertFrom-Json
        $persistedOwner = [Security.Principal.SecurityIdentifier]::new(
            [string] $identity.ownerSid
        ).Value
        if ($identity.version -ne 1 -or $persistedOwner -eq "S-1-5-18") {
            throw "INSTALL_IDENTITY_INVALID"
        }

        if (-not $acl.AreAccessRulesProtected -or
            $aclOwner -ne $persistedOwner) {
            throw "INSTALL_IDENTITY_INVALID"
        }
        $systemSid = "S-1-5-18"
        $seen = [Collections.Generic.HashSet[string]]::new(
            [StringComparer]::Ordinal
        )
        $serviceSidCount = 0
        foreach ($rule in @($acl.GetAccessRules(
                    $true,
                    $true,
                    [Security.Principal.SecurityIdentifier]
                ))) {
            $sid = $rule.IdentityReference.Value
            if ($sid -match "^S-1-5-80-(?:\d+-){4}\d+$") {
                $serviceSidCount += 1
            }
            elseif ($sid -notin @($systemSid, $persistedOwner)) {
                throw "INSTALL_IDENTITY_INVALID"
            }
            if (-not $seen.Add($sid) -or
                $rule.AccessControlType -ne
                    [Security.AccessControl.AccessControlType]::Allow -or
                $rule.IsInherited -or
                [long] $rule.FileSystemRights -ne
                    [long] [Security.AccessControl.FileSystemRights]::FullControl -or
                $rule.InheritanceFlags -ne
                    [Security.AccessControl.InheritanceFlags]::None -or
                $rule.PropagationFlags -ne
                    [Security.AccessControl.PropagationFlags]::None) {
                throw "INSTALL_IDENTITY_INVALID"
            }
        }
        $requiredCount = 3
        if ($serviceSidCount -ne 1 -or
            -not $seen.Contains($systemSid) -or
            -not $seen.Contains($persistedOwner) -or
            $seen.Count -ne $requiredCount) {
            throw "INSTALL_IDENTITY_INVALID"
        }
        if ($null -ne $requested -and $requested -ne $persistedOwner) {
            throw "INSTALL_OWNER_SID_MISMATCH"
        }
        return $persistedOwner
    }
    catch {
        if ($_.Exception.Message -in @(
                "INSTALL_OWNER_CONTEXT_REQUIRED",
                "INSTALL_OWNER_SID_MISMATCH"
            )) {
            throw
        }
        throw "INSTALL_IDENTITY_INVALID"
    }
}

function Copy-RestorePlanePackageFiles {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $PackageRoot,
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [object] $Manifest
    )

    foreach ($entry in @($Manifest.files)) {
        $source = Assert-RestorePlaneChildPath -Root $PackageRoot -Candidate (
            Join-Path $PackageRoot ([string] $entry.path)
        )
        $destination = Assert-RestorePlaneChildPath -Root $InstallRoot -Candidate (
            Join-Path $InstallRoot ([string] $entry.path)
        )
        $parent = Split-Path -Parent $destination
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
        Copy-Item -LiteralPath $source -Destination $destination -Force
    }
}

function Assert-RestorePlanePackageManifest {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $ManifestPath,
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $ExpectedSha256
    )

    try {
        $source = Resolve-RestorePlaneAbsolutePath -Path $ManifestPath
        $install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot
        if ($ExpectedSha256 -notmatch "^[0-9a-fA-F]{64}$") {
            throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
        }
        $expected = $ExpectedSha256.ToUpperInvariant()
        $destination = Assert-RestorePlaneChildPath -Root $install -Candidate (
            Join-Path $install "package-manifest.json"
        )
        if ($source.Equals(
                $destination,
                [StringComparison]::OrdinalIgnoreCase
            )) {
            throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
        }
        $null = Read-RestorePlaneTrustedPackageManifest `
            -ManifestPath $source `
            -ExpectedSha256 $expected
        $null = Read-RestorePlaneTrustedPackageManifest `
            -ManifestPath $destination `
            -ExpectedSha256 $expected
    }
    catch {
        throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
    }
}

function Copy-RestorePlanePackageManifest {
    [CmdletBinding(DefaultParameterSetName = "Path")]
    param(
        [Parameter(Mandatory = $true, ParameterSetName = "Path")]
        [string] $ManifestPath,
        [Parameter(Mandatory = $true, ParameterSetName = "Snapshot")]
        [byte[]] $TrustedManifestBytes,
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $ExpectedSha256
    )

    $install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot
    $destination = Assert-RestorePlaneChildPath -Root $install -Candidate (
        Join-Path $install "package-manifest.json"
    )
    if ($ExpectedSha256 -notmatch "^[0-9a-fA-F]{64}$") {
        throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
    }
    $expected = $ExpectedSha256.ToUpperInvariant()
    if ($PSCmdlet.ParameterSetName -eq "Path") {
        $source = Resolve-RestorePlaneAbsolutePath -Path $ManifestPath
        if ($source.Equals($destination, [StringComparison]::OrdinalIgnoreCase)) {
            throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
        }
        try {
            $trusted = Read-RestorePlaneTrustedPackageManifest `
                -ManifestPath $source `
                -ExpectedSha256 $expected
            $manifestBytes = $trusted.bytes
        }
        catch {
            throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
        }
    }
    else {
        try {
            $null = Read-RestorePlanePackageManifest `
                -ManifestBytes $TrustedManifestBytes
            if ((Get-RestorePlaneByteSha256 -Bytes $TrustedManifestBytes) -cne
                $expected) {
                throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
            }
            $manifestBytes = $TrustedManifestBytes
        }
        catch {
            throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
        }
    }
    $temporary = Assert-RestorePlaneChildPath -Root $install -Candidate (
        Join-Path $install (
            ".package-manifest." + [Guid]::NewGuid().ToString("N") + ".partial"
        )
    )
    try {
        [IO.File]::WriteAllBytes($temporary, $manifestBytes)
        Set-RestorePlaneFileAtomically `
            -TemporaryPath $temporary `
            -DestinationPath $destination
        $null = Read-RestorePlaneTrustedPackageManifest `
            -ManifestPath $destination `
            -ExpectedSha256 $expected
    }
    catch {
        throw "PACKAGE_INSTALLED_MANIFEST_INVALID"
    }
    finally {
        if (Test-Path -LiteralPath $temporary -PathType Leaf) {
            Remove-Item -LiteralPath $temporary -Force
        }
    }
}

function Move-RestorePlaneUnmanifestedInstallFiles {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $DataRoot,
        [Parameter(Mandatory = $true)] [object] $Manifest
    )

    $install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot -AllowMissing
    $data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot -AllowMissing
    Assert-RestorePlaneSeparatedRoots -InstallRoot $install -DataRoot $data

    if (-not (Test-Path -LiteralPath $install)) {
        return [PSCustomObject] @{
            quarantinedCount = 0
            quarantinePath = $null
        }
    }
    $installMetadata = Get-Item -LiteralPath $install -Force
    if (-not $installMetadata.PSIsContainer -or
        $installMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "INSTALL_TREE_REPARSE_POINT_BLOCKED"
    }

    $manifestedPaths = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    foreach ($entry in @($Manifest.files)) {
        $null = $manifestedPaths.Add(
            ([string] $entry.path).Replace("\", "/")
        )
    }
    $null = $manifestedPaths.Add("package-manifest.json")

    $items = [Collections.Generic.List[IO.FileSystemInfo]]::new()
    $directories = [Collections.Generic.Queue[IO.DirectoryInfo]]::new()
    $directories.Enqueue([IO.DirectoryInfo] $installMetadata)
    while ($directories.Count -gt 0) {
        $directory = $directories.Dequeue()
        foreach ($item in $directory.EnumerateFileSystemInfos()) {
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "INSTALL_TREE_REPARSE_POINT_BLOCKED"
            }
            $items.Add($item)
            if ($item -is [IO.DirectoryInfo]) {
                $directories.Enqueue([IO.DirectoryInfo] $item)
            }
        }
    }

    $extras = @(
        $items |
            Where-Object { $_ -is [IO.FileInfo] } |
            Where-Object {
                $relative = $_.FullName.Substring($install.Length).
                    TrimStart("\", "/").Replace("\", "/")
                -not $manifestedPaths.Contains($relative)
            }
    )
    if ($extras.Count -eq 0) {
        return [PSCustomObject] @{
            quarantinedCount = 0
            quarantinePath = $null
        }
    }

    New-Item -ItemType Directory -Path $data -Force | Out-Null
    $dataMetadata = Get-Item -LiteralPath $data -Force
    if (-not $dataMetadata.PSIsContainer -or
        $dataMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "INSTALL_DATA_ROOT_REPARSE_POINT_BLOCKED"
    }
    $updatesRoot = Assert-RestorePlaneChildPath -Root $data -Candidate (
        Join-Path $data "updates"
    )
    $quarantineBase = Assert-RestorePlaneChildPath -Root $data -Candidate (
        Join-Path $updatesRoot "install-tree-quarantine"
    )
    foreach ($directoryPath in @($updatesRoot, $quarantineBase)) {
        New-Item -ItemType Directory -Path $directoryPath -Force | Out-Null
        $directoryMetadata = Get-Item -LiteralPath $directoryPath -Force
        if (-not $directoryMetadata.PSIsContainer -or
            $directoryMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "INSTALL_DATA_ROOT_REPARSE_POINT_BLOCKED"
        }
    }
    $quarantine = Assert-RestorePlaneChildPath -Root $data -Candidate (
        Join-Path $quarantineBase (
            [DateTimeOffset]::UtcNow.ToString("yyyyMMddTHHmmssfffZ") + "-" +
            [Guid]::NewGuid().ToString("N")
        )
    )
    New-Item -ItemType Directory -Path $quarantine -Force | Out-Null

    foreach ($extra in $extras) {
        $relative = $extra.FullName.Substring($install.Length).
            TrimStart("\", "/")
        $destination = Assert-RestorePlaneChildPath -Root $quarantine -Candidate (
            Join-Path $quarantine $relative
        )
        New-Item -ItemType Directory -Path (Split-Path -Parent $destination) `
            -Force | Out-Null
        Move-Item -LiteralPath $extra.FullName -Destination $destination
    }

    foreach ($directory in @(
            $items |
                Where-Object { $_ -is [IO.DirectoryInfo] } |
                Sort-Object { $_.FullName.Length } -Descending
        )) {
        if ((Test-Path -LiteralPath $directory.FullName -PathType Container) -and
            @(Get-ChildItem -LiteralPath $directory.FullName -Force).Count -eq 0) {
            Remove-Item -LiteralPath $directory.FullName -Force
        }
    }

    return [PSCustomObject] @{
        quarantinedCount = $extras.Count
        quarantinePath = $quarantine
    }
}

function New-RestorePlaneDirectorySecurity {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [string] $ServiceSid,
        [ValidateSet("full", "read-execute")]
        [string] $ServiceAccess = "full"
    )

    try {
        $owner = [Security.Principal.SecurityIdentifier]::new($OwnerSid)
        $system = [Security.Principal.SecurityIdentifier]::new("S-1-5-18")
        $service = if ([string]::IsNullOrWhiteSpace($ServiceSid)) {
            $null
        }
        else {
            [Security.Principal.SecurityIdentifier]::new($ServiceSid)
        }
    }
    catch {
        throw "INSTALL_ACL_IDENTITY_INVALID"
    }
    if ($owner.Value -eq $system.Value -or
        ($null -ne $service -and
            ($service.Value -in @($system.Value, $owner.Value) -or
                $service.Value -notmatch "^S-1-5-80-(?:\d+-){4}\d+$"))) {
        throw "INSTALL_ACL_IDENTITY_INVALID"
    }

    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner($owner)
    $inheritance = [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
        [Security.AccessControl.InheritanceFlags]::ObjectInherit
    $full = [Security.AccessControl.FileSystemRights]::FullControl
    foreach ($identity in @($system, $owner)) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
                $identity,
                $full,
                $inheritance,
                [Security.AccessControl.PropagationFlags]::None,
                [Security.AccessControl.AccessControlType]::Allow
            ))
    }
    if ($null -ne $service) {
        $serviceRights = if ($ServiceAccess -eq "full") {
            $full
        }
        else {
            [Security.AccessControl.FileSystemRights]::ReadAndExecute
        }
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
                $service,
                $serviceRights,
                $inheritance,
                [Security.AccessControl.PropagationFlags]::None,
                [Security.AccessControl.AccessControlType]::Allow
            ))
    }
    return $acl
}

function Assert-RestorePlaneTrustedParentAcl {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $ParentPath,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [string] $ServiceSid,
        [string] $TrustedAncestorRoot
    )

    try {
        $resolved = Resolve-RestorePlaneAbsolutePath `
            -Path $ParentPath `
            -AllowVolumeRoot
        $trustedRoot = if ([string]::IsNullOrWhiteSpace($TrustedAncestorRoot)) {
            [IO.Path]::GetPathRoot($resolved)
        }
        else {
            Resolve-RestorePlaneAbsolutePath `
                -Path $TrustedAncestorRoot `
                -AllowVolumeRoot
        }
        $trustedPrefix = $trustedRoot.TrimEnd(
            [IO.Path]::DirectorySeparatorChar,
            [IO.Path]::AltDirectorySeparatorChar
        ) + [IO.Path]::DirectorySeparatorChar
        if (-not $resolved.Equals(
                $trustedRoot,
                [StringComparison]::OrdinalIgnoreCase
            ) -and
            -not $resolved.StartsWith(
                $trustedPrefix,
                [StringComparison]::OrdinalIgnoreCase
            )) {
            throw "INSTALL_PARENT_ACL_INVALID"
        }
        $allowed = [Collections.Generic.HashSet[string]]::new(
            [StringComparer]::OrdinalIgnoreCase
        )
        foreach ($sid in @(
                "S-1-5-18",
                "S-1-5-32-544",
                $OwnerSid,
                $ServiceSid,
                "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
            )) {
            if (-not [string]::IsNullOrWhiteSpace($sid)) {
                $canonical = [Security.Principal.SecurityIdentifier]::new(
                    $sid
                ).Value
                $null = $allowed.Add($canonical)
            }
        }
        $mutationRights = [long] (
            [Security.AccessControl.FileSystemRights]::Delete -bor
            [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor
            [Security.AccessControl.FileSystemRights]::ChangePermissions -bor
            [Security.AccessControl.FileSystemRights]::TakeOwnership
        )
        $current = $resolved
        while (-not [string]::IsNullOrWhiteSpace($current)) {
            $metadata = Get-Item -LiteralPath $current -Force
            if (-not $metadata.PSIsContainer -or
                $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "INSTALL_PARENT_ACL_INVALID"
            }
            $acl = Get-Acl -LiteralPath $current
            $parentOwner = $acl.GetOwner(
                [Security.Principal.SecurityIdentifier]
            ).Value
            if (-not $allowed.Contains($parentOwner)) {
                throw "INSTALL_PARENT_ACL_INVALID"
            }
            foreach ($rule in @($acl.Access)) {
                if ($rule.AccessControlType -ne
                        [Security.AccessControl.AccessControlType]::Allow -or
                    $rule.PropagationFlags -band
                        [Security.AccessControl.PropagationFlags]::InheritOnly) {
                    continue
                }
                $sid = $rule.IdentityReference.Translate(
                    [Security.Principal.SecurityIdentifier]
                ).Value
                if (-not $allowed.Contains($sid) -and
                    (([long] $rule.FileSystemRights) -band $mutationRights) -ne 0) {
                    throw "INSTALL_PARENT_ACL_INVALID"
                }
            }
            if ($current.Equals(
                    $trustedRoot,
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                break
            }
            $current = Split-Path -Parent $current
        }
    }
    catch {
        throw "INSTALL_PARENT_ACL_INVALID"
    }
}

function New-RestorePlaneProtectedDirectoryAtomically {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [string] $ServiceSid,
        [string] $AllowedParentServiceSid,
        [string] $TrustedAncestorRoot
    )

    $resolved = Resolve-RestorePlaneAbsolutePath -Path $Path -AllowMissing
    $parent = Resolve-RestorePlaneAbsolutePath `
        -Path (Split-Path -Parent $resolved) `
        -AllowVolumeRoot
    Assert-RestorePlaneTrustedParentAcl `
        -ParentPath $parent `
        -OwnerSid $OwnerSid `
        -ServiceSid $AllowedParentServiceSid `
        -TrustedAncestorRoot $TrustedAncestorRoot
    $staging = Join-Path $parent (
        ".restoreplane-protected-" + [Guid]::NewGuid().ToString("N")
    )
    $parentLock = Enter-RestorePlanePathMutationLock `
        -Path $parent `
        -AllowVolumeRoot
    try {
        if (Test-Path -LiteralPath $resolved) {
            throw "INSTALL_PROTECTED_DIRECTORY_EXISTS"
        }
        $security = New-RestorePlaneDirectorySecurity `
            -OwnerSid $OwnerSid `
            -ServiceSid $ServiceSid
        [IO.DirectoryInfo]::new($staging).Create($security)
        Assert-RestorePlaneProtectedAcl `
            -Path $staging `
            -OwnerSid $OwnerSid `
            -ServiceSid $ServiceSid
    }
    finally {
        $parentLock.Dispose()
    }
    try {
        Move-RestorePlaneDirectoryByHandleAtomically `
            -SourcePath $staging `
            -DestinationPath $resolved
    }
    finally {
        if (Test-Path -LiteralPath $staging -PathType Container) {
            Remove-Item -LiteralPath $staging -Recurse -Force
        }
    }
    Assert-RestorePlaneProtectedAcl `
        -Path $resolved `
        -OwnerSid $OwnerSid `
        -ServiceSid $ServiceSid
    return $resolved
}

function Assert-RestorePlaneSupportedRootParent {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $DefaultParent,
        [string] $TrustedAncestorRoot
    )

    if (-not [string]::IsNullOrWhiteSpace($TrustedAncestorRoot)) {
        return
    }
    $resolved = Resolve-RestorePlaneAbsolutePath -Path $Path -AllowMissing
    $parent = Resolve-RestorePlaneAbsolutePath `
        -Path (Split-Path -Parent $resolved) `
        -AllowVolumeRoot
    $default = Resolve-RestorePlaneAbsolutePath -Path $DefaultParent
    $volumeRoot = [IO.Path]::GetPathRoot($resolved)
    if (-not $parent.Equals(
            $default,
            [StringComparison]::OrdinalIgnoreCase
        ) -and
        -not $parent.Equals(
            $volumeRoot,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw "INSTALL_CUSTOM_ROOT_PARENT_UNTRUSTED"
    }
}

function Assert-RestorePlaneSupportedRootLayout {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $DataRoot
    )

    try {
        $install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot -AllowMissing
        $data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot -AllowMissing
        Assert-RestorePlaneSeparatedRoots -InstallRoot $install -DataRoot $data
        Assert-RestorePlaneSupportedRootParent `
            -Path $install `
            -DefaultParent ([Environment]::GetFolderPath(
                [Environment+SpecialFolder]::ProgramFiles
            ))
        Assert-RestorePlaneSupportedRootParent `
            -Path $data `
            -DefaultParent ([Environment]::GetFolderPath(
                [Environment+SpecialFolder]::CommonApplicationData
            ))
    }
    catch {
        if ($_.Exception.Message -eq "INSTALL_DATA_ROOT_OVERLAP") {
            throw
        }
        throw "INSTALL_CUSTOM_ROOT_PARENT_UNTRUSTED"
    }
}

function Get-RestorePlaneDataControlDirectoryNames {
    return @(
        "secrets",
        "spool",
        "control-backups",
        "support-bundles",
        "telemetry-archives",
        "updates"
    )
}

function Get-RestorePlaneInstallRootProvenancePath {
    param([Parameter(Mandatory = $true)] [string] $DataRoot)

    return Assert-RestorePlaneChildPath -Root $DataRoot -Candidate (
        Join-Path $DataRoot "install-root-provenance.json"
    )
}

function Read-RestorePlaneInstallRootProvenanceFile {
    param([Parameter(Mandatory = $true)] [string] $Path)

    $metadata = Get-Item -LiteralPath $Path -Force
    if ($metadata.PSIsContainer -or
        $metadata.Length -le 0 -or
        $metadata.Length -gt 64KB -or
        $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "INSTALL_ROOT_PROVENANCE_INVALID"
    }
    $stream = [IO.File]::Open(
        $metadata.FullName,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read
    )
    try {
        if ($stream.Length -le 0 -or $stream.Length -gt 64KB) {
            throw "INSTALL_ROOT_PROVENANCE_INVALID"
        }
        $bytes = [byte[]]::new([int] $stream.Length)
        $offset = 0
        while ($offset -lt $bytes.Length) {
            $read = $stream.Read($bytes, $offset, $bytes.Length - $offset)
            if ($read -le 0) {
                throw "INSTALL_ROOT_PROVENANCE_INVALID"
            }
            $offset += $read
        }
        $json = [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
        return $json | ConvertFrom-Json
    }
    finally {
        $stream.Dispose()
    }
}

function Test-RestorePlaneExactPropertySet {
    param(
        [Parameter(Mandatory = $true)] [object] $Value,
        [Parameter(Mandatory = $true)] [string[]] $Expected
    )

    $actual = @($Value.PSObject.Properties.Name)
    if ($actual.Count -ne $Expected.Count) {
        return $false
    }
    foreach ($name in $Expected) {
        if ($name -cnotin $actual) {
            return $false
        }
    }
    return $true
}

function Assert-RestorePlaneInstallRootProvenance {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $DataRoot,
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [Parameter(Mandatory = $true)] [string] $ServiceSid,
        [switch] $AllowInstallRootReplacement
    )

    try {
        $data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot
        $install = Resolve-RestorePlaneAbsolutePath `
            -Path $InstallRoot `
            -AllowMissing:$AllowInstallRootReplacement
        Assert-RestorePlaneSeparatedRoots -InstallRoot $install -DataRoot $data
        $provenancePath = Get-RestorePlaneInstallRootProvenancePath -DataRoot $data
        Assert-RestorePlaneProtectedAcl `
            -Path $provenancePath `
            -OwnerSid $OwnerSid `
            -ServiceSid $ServiceSid
        $provenanceLock = Enter-RestorePlanePathMutationLock -Path $provenancePath
        try {
            $provenance = Read-RestorePlaneInstallRootProvenanceFile `
                -Path $provenancePath
            if (-not (Test-RestorePlaneExactPropertySet `
                    -Value $provenance `
                    -Expected @(
                        "schemaVersion",
                        "dataRoot",
                        "installRoot",
                        "controlDirectories"
                    )) -or
                $provenance.schemaVersion -isnot [int] -or
                $provenance.schemaVersion -ne 1 -or
                $null -eq $provenance.dataRoot -or
                $null -eq $provenance.installRoot -or
                $null -eq $provenance.controlDirectories -or
                -not (Test-RestorePlaneExactPropertySet `
                    -Value $provenance.dataRoot `
                    -Expected @("path", "identity")) -or
                -not (Test-RestorePlaneExactPropertySet `
                    -Value $provenance.installRoot `
                    -Expected @("path", "identity"))) {
                throw "INSTALL_ROOT_PROVENANCE_INVALID"
            }
            if (-not ([string] $provenance.dataRoot.path).Equals(
                    $data,
                    [StringComparison]::OrdinalIgnoreCase
                ) -or
                -not ([string] $provenance.installRoot.path).Equals(
                    $install,
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                throw "INSTALL_ROOT_PROVENANCE_INVALID"
            }

            $expectedIdentities = [ordered] @{
                dataRoot = Get-RestorePlanePathIdentity -Path $data
            }
            if ([string] $provenance.dataRoot.identity -cne
                    $expectedIdentities.dataRoot) {
                throw "INSTALL_ROOT_PROVENANCE_INVALID"
            }
            if (-not $AllowInstallRootReplacement -and
                [string] $provenance.installRoot.identity -cne
                    (Get-RestorePlanePathIdentity -Path $install)) {
                throw "INSTALL_ROOT_PROVENANCE_INVALID"
            }

            $expectedNames = @(Get-RestorePlaneDataControlDirectoryNames)
            $recordedControls = @($provenance.controlDirectories)
            if ($recordedControls.Count -ne $expectedNames.Count) {
                throw "INSTALL_ROOT_PROVENANCE_INVALID"
            }
            $seen = [Collections.Generic.HashSet[string]]::new(
                [StringComparer]::OrdinalIgnoreCase
            )
            foreach ($control in $recordedControls) {
                if (-not (Test-RestorePlaneExactPropertySet `
                        -Value $control `
                        -Expected @("name", "path", "identity"))) {
                    throw "INSTALL_ROOT_PROVENANCE_INVALID"
                }
                $name = [string] $control.name
                if ($name -notin $expectedNames -or -not $seen.Add($name)) {
                    throw "INSTALL_ROOT_PROVENANCE_INVALID"
                }
                $path = Assert-RestorePlaneChildPath -Root $data -Candidate (
                    Join-Path $data $name
                )
                if (-not ([string] $control.path).Equals(
                        $path,
                        [StringComparison]::OrdinalIgnoreCase
                    ) -or
                    [string] $control.identity -cne
                        (Get-RestorePlanePathIdentity -Path $path)) {
                    throw "INSTALL_ROOT_PROVENANCE_INVALID"
                }
                Assert-RestorePlaneProtectedAcl `
                    -Path $path `
                    -OwnerSid $OwnerSid `
                    -ServiceSid $ServiceSid
            }
            foreach ($identity in @(
                    [string] $provenance.dataRoot.identity,
                    [string] $provenance.installRoot.identity
                ) + @($recordedControls | ForEach-Object {
                        [string] $_.identity
                    })) {
                if ($identity -cnotmatch
                    "^v1:[0-9a-f]{16}:[0-9a-f]{32}$") {
                    throw "INSTALL_ROOT_PROVENANCE_INVALID"
                }
            }
            Assert-RestorePlaneProtectedAcl `
                -Path $data `
                -OwnerSid $OwnerSid `
                -ServiceSid $ServiceSid
            if (-not $AllowInstallRootReplacement) {
                Assert-RestorePlaneProtectedAcl `
                    -Path $install `
                    -OwnerSid $OwnerSid `
                    -ServiceSid $ServiceSid `
                    -ServiceAccess "read-execute" `
                    -Recurse
            }
        }
        finally {
            $provenanceLock.Dispose()
        }
    }
    catch {
        throw "INSTALL_ROOT_PROVENANCE_INVALID"
    }
}

function Write-RestorePlaneInstallRootProvenance {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $DataRoot,
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [Parameter(Mandatory = $true)] [string] $ServiceSid,
        [switch] $AllowInitialMint
    )

    try {
        $data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot
        $install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot
        Assert-RestorePlaneSeparatedRoots -InstallRoot $install -DataRoot $data
        $provenancePath = Get-RestorePlaneInstallRootProvenancePath -DataRoot $data
        if (-not (Test-Path -LiteralPath $provenancePath -PathType Leaf) -and
            -not $AllowInitialMint) {
            throw "INSTALL_ROOT_PROVENANCE_INVALID"
        }
        Assert-RestorePlaneProtectedAcl `
            -Path $data `
            -OwnerSid $OwnerSid `
            -ServiceSid $ServiceSid
        Assert-RestorePlaneProtectedAcl `
            -Path $install `
            -OwnerSid $OwnerSid `
            -ServiceSid $ServiceSid `
            -ServiceAccess "read-execute" `
            -Recurse

        $controls = [Collections.Generic.List[object]]::new()
        foreach ($name in @(Get-RestorePlaneDataControlDirectoryNames)) {
            $path = Assert-RestorePlaneChildPath -Root $data -Candidate (
                Join-Path $data $name
            )
            Assert-RestorePlaneProtectedAcl `
                -Path $path `
                -OwnerSid $OwnerSid `
                -ServiceSid $ServiceSid
            $controls.Add([ordered] @{
                    name = $name
                    path = $path
                    identity = Get-RestorePlanePathIdentity -Path $path
                })
        }
        Write-RestorePlaneJsonAtomic -Path $provenancePath -Value ([ordered] @{
                schemaVersion = 1
                dataRoot = [ordered] @{
                    path = $data
                    identity = Get-RestorePlanePathIdentity -Path $data
                }
                installRoot = [ordered] @{
                    path = $install
                    identity = Get-RestorePlanePathIdentity -Path $install
                }
                controlDirectories = $controls
            })
        Set-RestorePlaneProtectedAcl `
            -Path $provenancePath `
            -OwnerSid $OwnerSid `
            -ServiceSid $ServiceSid
        Assert-RestorePlaneInstallRootProvenance `
            -DataRoot $data `
            -InstallRoot $install `
            -OwnerSid $OwnerSid `
            -ServiceSid $ServiceSid
    }
    catch {
        throw "INSTALL_ROOT_PROVENANCE_INVALID"
    }
}

function Initialize-RestorePlaneDataRoot {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $DataRoot,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [Parameter(Mandatory = $true)] [string] $ServiceSid,
        [string] $InstallRoot,
        [switch] $AllowInstallRootReplacement,
        [string] $TrustedAncestorRoot
    )

    try {
        $resolved = Resolve-RestorePlaneAbsolutePath -Path $DataRoot -AllowMissing
        Assert-RestorePlaneSupportedRootParent `
            -Path $resolved `
            -DefaultParent ([Environment]::GetFolderPath(
                [Environment+SpecialFolder]::CommonApplicationData
            )) `
            -TrustedAncestorRoot $TrustedAncestorRoot
        $created = -not (Test-Path -LiteralPath $resolved)
        if (-not $created) {
            if ([string]::IsNullOrWhiteSpace($InstallRoot)) {
                throw "INSTALL_ROOT_PROVENANCE_INVALID"
            }
            Assert-RestorePlaneInstallRootProvenance `
                -DataRoot $resolved `
                -InstallRoot $InstallRoot `
                -OwnerSid $OwnerSid `
                -ServiceSid $ServiceSid `
                -AllowInstallRootReplacement:$AllowInstallRootReplacement
            Assert-RestorePlaneProtectedAcl `
                -Path $resolved `
                -OwnerSid $OwnerSid `
                -ServiceSid $ServiceSid
        }
        else {
            $null = New-RestorePlaneProtectedDirectoryAtomically `
                -Path $resolved `
                -OwnerSid $OwnerSid `
                -ServiceSid $ServiceSid `
                -TrustedAncestorRoot $TrustedAncestorRoot
        }
        foreach ($name in @(Get-RestorePlaneDataControlDirectoryNames)) {
            $directory = Assert-RestorePlaneChildPath `
                -Root $resolved `
                -Candidate (Join-Path $resolved $name)
            if (Test-Path -LiteralPath $directory) {
                Assert-RestorePlaneProtectedAcl `
                    -Path $directory `
                    -OwnerSid $OwnerSid `
                    -ServiceSid $ServiceSid
            }
            else {
                $null = New-RestorePlaneProtectedDirectoryAtomically `
                    -Path $directory `
                    -OwnerSid $OwnerSid `
                    -ServiceSid $ServiceSid `
                    -AllowedParentServiceSid $ServiceSid `
                    -TrustedAncestorRoot $resolved
            }
        }
        return [PSCustomObject] @{
            created = $created
            identity = Get-RestorePlanePathIdentity -Path $resolved
        }
    }
    catch {
        if ($_.Exception.Message -eq "INSTALL_CUSTOM_ROOT_PARENT_UNTRUSTED") {
            throw
        }
        throw "INSTALL_DATA_ROOT_INVALID"
    }
}

function Initialize-RestorePlaneInstallRoot {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $InstallRoot,
        [Parameter(Mandatory = $true)] [string] $DataRoot,
        [Parameter(Mandatory = $true)] [string] $OwnerSid,
        [string] $ServiceSid,
        [string] $TrustedAncestorRoot
    )

    $install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot -AllowMissing
    $data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot
    Assert-RestorePlaneSeparatedRoots -InstallRoot $install -DataRoot $data
    try {
        Assert-RestorePlaneSupportedRootParent `
            -Path $install `
            -DefaultParent ([Environment]::GetFolderPath(
                [Environment+SpecialFolder]::ProgramFiles
            )) `
            -TrustedAncestorRoot $TrustedAncestorRoot
    }
    catch {
        if ($_.Exception.Message -eq "INSTALL_CUSTOM_ROOT_PARENT_UNTRUSTED") {
            throw
        }
        throw "INSTALL_ROOT_REPLACEMENT_FAILED"
    }
    $quarantinePath = $null
    if (Test-Path -LiteralPath $install) {
        try {
            $installParent = Resolve-RestorePlaneAbsolutePath `
                -Path (Split-Path -Parent $install) `
                -AllowVolumeRoot
            Assert-RestorePlaneTrustedParentAcl `
                -ParentPath $installParent `
                -OwnerSid $OwnerSid `
                -TrustedAncestorRoot $TrustedAncestorRoot
            $quarantinePath = Join-Path $installParent (
                ".restoreplane-install-root-quarantine-" +
                    [Guid]::NewGuid().ToString("N")
            )
            Move-RestorePlaneDirectoryByHandleAtomically `
                -SourcePath $install `
                -DestinationPath $quarantinePath
            if (Test-Path -LiteralPath $install) {
                throw "INSTALL_ROOT_REPLACEMENT_FAILED"
            }
        }
        catch {
            throw "INSTALL_ROOT_REPLACEMENT_FAILED"
        }
    }

    if (-not (Test-Path -LiteralPath $install)) {
        try {
            $null = New-RestorePlaneProtectedDirectoryAtomically `
                -Path $install `
                -OwnerSid $OwnerSid `
                -TrustedAncestorRoot $TrustedAncestorRoot
        }
        catch {
            throw "INSTALL_ROOT_REPLACEMENT_FAILED"
        }
    }
    return [PSCustomObject] @{
        quarantinePath = $quarantinePath
    }
}

function Set-RestorePlaneFileAtomically {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $TemporaryPath,
        [Parameter(Mandatory = $true)] [string] $DestinationPath
    )

    $temporary = Resolve-RestorePlaneAbsolutePath -Path $TemporaryPath
    $destination = Resolve-RestorePlaneAbsolutePath `
        -Path $DestinationPath `
        -AllowMissing
    $temporaryParent = [IO.Path]::GetFullPath(
        (Split-Path -Parent $temporary)
    ).TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    )
    $destinationParent = [IO.Path]::GetFullPath(
        (Split-Path -Parent $destination)
    ).TrimEnd(
        [IO.Path]::DirectorySeparatorChar,
        [IO.Path]::AltDirectorySeparatorChar
    )
    if (-not $temporaryParent.Equals(
            $destinationParent,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw "INSTALL_ATOMIC_REPLACE_CROSS_DIRECTORY"
    }
    $temporaryMetadata = Get-Item -LiteralPath $temporary -Force
    if ($temporaryMetadata.PSIsContainer -or
        $temporaryMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "INSTALL_ATOMIC_REPLACE_INPUT_INVALID"
    }
    $parentMetadata = Get-Item -LiteralPath $destinationParent -Force
    if (-not $parentMetadata.PSIsContainer -or
        $parentMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "INSTALL_ATOMIC_REPLACE_PARENT_INVALID"
    }
    if (Test-Path -LiteralPath $destination) {
        $destinationMetadata = Get-Item -LiteralPath $destination -Force
        if ($destinationMetadata.PSIsContainer -or
            $destinationMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "INSTALL_ATOMIC_REPLACE_DESTINATION_INVALID"
        }
    }

    $stream = [IO.File]::Open(
        $temporary,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    try {
        $stream.Flush($true)
    }
    finally {
        $stream.Dispose()
    }
    if (Test-Path -LiteralPath $destination -PathType Leaf) {
        $replaceBackup = Join-Path $destinationParent (
            "." + [Guid]::NewGuid().ToString("N") + ".replace-backup"
        )
        [IO.File]::Replace(
            $temporary,
            $destination,
            $replaceBackup,
            $true
        )
        if (Test-Path -LiteralPath $replaceBackup -PathType Leaf) {
            Remove-Item -LiteralPath $replaceBackup -Force
        }
    }
    else {
        [IO.File]::Move($temporary, $destination)
    }
    $destinationStream = [IO.File]::Open(
        $destination,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::Read
    )
    try {
        $destinationStream.Flush($true)
    }
    finally {
        $destinationStream.Dispose()
    }
}

function Write-RestorePlaneJsonAtomic {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $temporary = Join-Path $parent ("." + [Guid]::NewGuid().ToString("N") + ".partial")
    try {
        $json = $Value | ConvertTo-Json -Depth 32
        [IO.File]::WriteAllText(
            $temporary,
            $json,
            [Text.UTF8Encoding]::new($false)
        )
        Set-RestorePlaneFileAtomically `
            -TemporaryPath $temporary `
            -DestinationPath $Path
    }
    finally {
        if (Test-Path -LiteralPath $temporary -PathType Leaf) {
            Remove-Item -LiteralPath $temporary -Force
        }
    }
}

Export-ModuleMember -Function @(
    "Resolve-RestorePlaneAbsolutePath",
    "Assert-RestorePlaneSeparatedRoots",
    "Assert-RestorePlaneChildPath",
    "Read-RestorePlanePackageManifest",
    "Read-RestorePlaneTrustedPackageManifest",
    "Test-RestorePlanePackage",
    "Assert-RestorePlaneAdministrator",
    "Invoke-RestorePlaneNative",
    "Get-RestorePlaneServiceSid",
    "Enter-RestorePlanePathMutationLock",
    "Enter-RestorePlaneInstallTreeLock",
    "Move-RestorePlaneDirectoryByHandleAtomically",
    "Get-RestorePlanePathIdentity",
    "Set-RestorePlaneProtectedAcl",
    "Assert-RestorePlaneProtectedAcl",
    "Assert-RestorePlaneSupportedRootLayout",
    "Initialize-RestorePlaneDataRoot",
    "Initialize-RestorePlaneInstallRoot",
    "Assert-RestorePlaneInstallRootProvenance",
    "Write-RestorePlaneInstallRootProvenance",
    "Resolve-RestorePlaneOwnerSid",
    "Copy-RestorePlanePackageFiles",
    "Copy-RestorePlanePackageManifest",
    "Assert-RestorePlanePackageManifest",
    "Move-RestorePlaneUnmanifestedInstallFiles",
    "Set-RestorePlaneFileAtomically",
    "Write-RestorePlaneJsonAtomic"
)
