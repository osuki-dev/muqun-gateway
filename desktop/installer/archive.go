package main

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"compress/gzip"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

const macApp = "Muqun Desktop.app"
const linuxPackage = "muqun-desktop-linux-x86_64"

func validMember(name string, roots ...string) bool {
	name = strings.TrimSuffix(name, "/")
	if !fs.ValidPath(name) {
		return false
	}
	root, _, _ := strings.Cut(name, "/")
	for _, r := range roots {
		if root == r {
			return true
		}
	}
	return false
}

func run(env []string, program string, args ...string) error {
	cmd := exec.Command(program, args...)
	cmd.Env = env
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func stageLinux(archive, dest, txn, payload string) ([]string, error) {
	f, err := os.Open(archive)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	gz, err := gzip.NewReader(f)
	if err != nil {
		return nil, err
	}
	defer gz.Close()
	stream := tar.NewReader(gz)
	for {
		h, err := stream.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, err
		}
		if !validMember(h.Name, linuxPackage) || (h.Typeflag != tar.TypeReg && h.Typeflag != tar.TypeDir) {
			return nil, fmt.Errorf("unexpected Linux archive entry: %s", h.Name)
		}
	}
	if err := run(nil, "tar", "-xzf", archive, "-C", txn); err != nil {
		return nil, err
	}
	env := append(os.Environ(), "PREFIX="+payload, "MUQUN_DESKTOP_LAUNCH_PREFIX="+dest)
	if err := run(env, "bash", filepath.Join(txn, linuxPackage, "desktop/scripts/install-local.sh"), "--no-build"); err != nil {
		return nil, err
	}
	exe, err := os.Stat(filepath.Join(payload, "bin/muqun-desktop"))
	if err != nil {
		return nil, err
	}
	if !exe.Mode().IsRegular() {
		return nil, errors.New("package missing Desktop executable")
	}
	launcher, err := os.ReadFile(filepath.Join(payload, "share/applications/muqun-desktop.desktop"))
	if err != nil {
		return nil, err
	}
	if bytes.Contains(launcher, []byte(payload)) {
		return nil, errors.New("launcher points to staging directory")
	}
	var paths []string
	err = filepath.WalkDir(payload, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		if !d.Type().IsRegular() {
			return fmt.Errorf("unexpected staged file: %s", path)
		}
		if d.Name() == "mimeinfo.cache" || d.Name() == "icon-theme.cache" {
			return nil
		}
		rel, err := filepath.Rel(payload, path)
		if err != nil {
			return err
		}
		paths = append(paths, rel)
		return nil
	})
	return paths, err
}

func stageMac(archive, dest, payload string) ([]string, error) {
	z, err := zip.OpenReader(archive)
	if err != nil {
		return nil, err
	}
	defer z.Close()
	for _, file := range z.File {
		if !validMember(file.Name, macApp, "__MACOSX") || !(file.Mode().IsRegular() || file.Mode().IsDir()) {
			return nil, fmt.Errorf("unexpected macOS archive entry: %s", file.Name)
		}
	}
	if err = run(nil, "ditto", "-x", "-k", archive, payload); err != nil {
		return nil, err
	}
	app := filepath.Join(payload, macApp)
	for _, bundle := range []string{app, filepath.Join(dest, macApp)} {
		i, err := infoAt(bundle)
		if err != nil {
			return nil, err
		}
		if i == nil && bundle != app {
			continue
		}
		if i == nil || !i.IsDir() {
			return nil, fmt.Errorf("unexpected app at %s", bundle)
		}
		id, err := exec.Command("plutil", "-extract", "CFBundleIdentifier", "raw", "-o", "-", filepath.Join(bundle, "Contents/Info.plist")).Output()
		if err != nil || strings.TrimSpace(string(id)) != "com.muqun.desktop" {
			return nil, fmt.Errorf("unexpected app identity at %s", bundle)
		}
	}
	if err = run(nil, "codesign", "--verify", "--deep", "--strict", app); err != nil {
		return nil, err
	}
	if err = run(nil, "spctl", "--assess", "--type", "execute", app); err != nil {
		return nil, err
	}
	return []string{macApp}, nil
}

func stage(system, archive, dest, txn, payload string) ([]string, error) {
	switch system {
	case "Linux":
		return stageLinux(archive, dest, txn, payload)
	case "Darwin":
		return stageMac(archive, dest, payload)
	}
	return nil, fmt.Errorf("unsupported OS: %s", system)
}
