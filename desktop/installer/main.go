// muqun-desktop-install activates the archive verified by install-desktop.sh.
package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"syscall"
)

type entry struct {
	path    string
	existed bool
}

// Preserve the journal format of earlier Desktop installers.
func (e entry) MarshalJSON() ([]byte, error) { return json.Marshal([]any{e.path, e.existed}) }
func (e *entry) UnmarshalJSON(b []byte) error {
	var pair []json.RawMessage
	if err := json.Unmarshal(b, &pair); err != nil {
		return err
	}
	if len(pair) != 2 {
		return errors.New("invalid install journal")
	}
	if err := json.Unmarshal(pair[0], &e.path); err != nil {
		return err
	}
	if !fs.ValidPath(e.path) || e.path == "." || strings.HasPrefix(e.path, ".muqun-desktop-install") {
		return errors.New("invalid install path")
	}
	return json.Unmarshal(pair[1], &e.existed)
}

func infoAt(path string) (fs.FileInfo, error) {
	info, err := os.Lstat(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	return info, err
}

// Prefixes may themselves be symlinks. Below the resolved prefix, no file
// installation may traverse a symlink or a non-directory.
func checkedParent(dest, rel string) error {
	dir := dest
	parts := strings.Split(rel, "/")
	for _, p := range parts[:len(parts)-1] {
		dir = filepath.Join(dir, p)
		i, err := infoAt(dir)
		if err != nil {
			return err
		}
		if i == nil {
			return nil
		}
		if !i.IsDir() {
			return fmt.Errorf("invalid package destination parent: %s", dir)
		}
	}
	return nil
}

func recoverInstall(dest, txn string) error {
	info, err := infoAt(txn)
	if err != nil || info == nil {
		return err
	}
	if !info.IsDir() {
		return fmt.Errorf("invalid staging directory: %s", txn)
	}
	journal := filepath.Join(txn, "journal.json")
	data, err := os.ReadFile(journal)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	if err == nil {
		var plan []entry
		if err := json.Unmarshal(data, &plan); err != nil {
			return err
		}
		for i := len(plan) - 1; i >= 0; i-- {
			e := plan[i]
			if err := checkedParent(dest, e.path); err != nil {
				return err
			}
			target := filepath.Join(dest, e.path)
			backup := filepath.Join(txn, "old", e.path)
			old, err := infoAt(backup)
			if err != nil {
				return err
			}
			if old != nil {
				if err := os.RemoveAll(target); err != nil {
					return err
				}
				if err := os.Rename(backup, target); err != nil {
					return err
				}
			} else if !e.existed {
				if err := os.RemoveAll(target); err != nil {
					return err
				}
			}
		}
	}
	return os.RemoveAll(txn)
}

func activate(dest, txn, payload string, paths []string, system string) error {
	plan := make([]entry, 0, len(paths))
	for _, rel := range paths {
		if !fs.ValidPath(rel) || rel == "." || strings.HasPrefix(rel, ".muqun-desktop-install") {
			return fmt.Errorf("invalid package path: %s", rel)
		}
		if err := checkedParent(dest, rel); err != nil {
			return err
		}
		target := filepath.Join(dest, rel)
		i, err := infoAt(target)
		if err != nil {
			return err
		}
		if i != nil && i.IsDir() && !(system == "Darwin" && rel == macApp) {
			return fmt.Errorf("expected file at %s", target)
		}
		plan = append(plan, entry{rel, i != nil})
	}
	data, err := json.Marshal(plan)
	if err != nil {
		return err
	}
	pending := filepath.Join(txn, "journal.tmp")
	f, err := os.OpenFile(pending, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err != nil {
		return err
	}
	if closeErr != nil {
		return closeErr
	}
	journal := filepath.Join(txn, "journal.json")
	if err = os.Rename(pending, journal); err != nil {
		return err
	}
	for _, e := range plan {
		target := filepath.Join(dest, e.path)
		if err = os.MkdirAll(filepath.Dir(target), 0755); err != nil {
			return err
		}
		if e.existed {
			backup := filepath.Join(txn, "old", e.path)
			if err = os.MkdirAll(filepath.Dir(backup), 0700); err != nil {
				return err
			}
			if err = os.Rename(target, backup); err != nil {
				return err
			}
		}
		if err = os.Rename(filepath.Join(payload, e.path), target); err != nil {
			return err
		}
	}
	return os.Remove(journal)
}

func verifyVersion(system, directory, version string) error {
	marker := "share/muqun-desktop/installers/VERSION"
	if system == "Darwin" {
		marker = filepath.Join(macApp, "Contents/Resources/installers/VERSION")
	}
	f, err := os.Open(filepath.Join(directory, marker))
	if err != nil {
		return err
	}
	data, err := io.ReadAll(io.LimitReader(f, int64(len(version)+2)))
	f.Close()
	if err != nil {
		return err
	}
	if string(data) != version+"\n" {
		return fmt.Errorf("expected Desktop version %s", version)
	}
	return nil
}

func install(system, archive, dest, version string) (result error) {
	full, err := filepath.Abs(dest)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(full, 0755); err != nil {
		return err
	}
	dest, err = filepath.EvalSymlinks(full)
	if err != nil {
		return err
	}
	lock, err := os.OpenFile(filepath.Join(dest, ".muqun-desktop-install.lock"), os.O_CREATE|os.O_RDWR|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		return fmt.Errorf("cannot lock Desktop installation: %w", err)
	}
	txn := filepath.Join(dest, ".muqun-desktop-install")
	if err = recoverInstall(dest, txn); err != nil {
		return err
	}
	if current := os.Getenv("MUQUN_DESKTOP_REPLACE_VERSION"); current != "" {
		if err := verifyVersion(system, dest, current); err != nil {
			return fmt.Errorf("Desktop installation changed since this update started: %w", err)
		}
	}
	if err = os.Mkdir(txn, 0700); err != nil {
		return err
	}
	defer func() { result = errors.Join(result, recoverInstall(dest, txn)) }()
	payload := filepath.Join(txn, "payload")
	if err = os.Mkdir(payload, 0700); err != nil {
		return err
	}
	paths, err := stage(system, archive, dest, txn, payload)
	if err != nil {
		return err
	}
	if err := verifyVersion(system, payload, version); err != nil {
		return fmt.Errorf("staged Desktop version differs from requested release: %w", err)
	}
	return activate(dest, txn, payload, paths, system)
}

func main() {
	if len(os.Args) != 5 {
		fmt.Fprintln(os.Stderr, "usage: muqun-desktop-install OS ARCHIVE DEST VERSION")
		os.Exit(2)
	}
	if err := install(os.Args[1], os.Args[2], os.Args[3], os.Args[4]); err != nil {
		fmt.Fprintln(os.Stderr, "Muqun installer:", err)
		os.Exit(1)
	}
}
