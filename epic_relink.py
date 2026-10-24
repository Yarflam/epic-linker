#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
epic_relink.py — Régénère les fichiers .item du launcher Epic Games pour des jeux
déjà présents sur le disque mais "invisibles" pour le launcher (manifests perdus).

Principe : chaque jeu installé contient un dossier .egstore avec :
  - <GUID>.manifest  : manifest binaire Epic (format DM, magic 0x44BEC00C, zlib)
                     -> donne AppName, AppVersionString, LaunchExecutable
                     -> son nom de fichier = InstallationGuid
                     -> son SHA1 = ManifestHash (champ du .item)
  - <GUID>.mancpn    : petit JSON -> CatalogNamespace, CatalogItemId, AppName
  - [optionnel] <app>appconfig.json -> DeploymentId

Le script reconstruit ensuite :
  1. C:\\ProgramData\\Epic\\EpicGamesLauncher\\Data\\Manifests\\<GUID>.item
  2. L'entrée correspondante dans LauncherInstalled.dat (UTF-16LE)

Usage :
  python epic_relink.py                      # scan de D:\\EpicGames, mode réel
  python epic_relink.py --dry-run            # voir ce qui serait fait
  python epic_relink.py --dirs D:\\Jeux E:\\Epic  # autres répertoires
  python epic_relink.py --force              # régénère même les .item existants

Le script est idempotent : relancer ne fait rien si tout est à jour, et "relinke"
automatiquement les .item dont les chemins ne correspondent plus à l'emplacement
réel du jeu (déplacement de dossier).
"""

import argparse
import base64
import hashlib
import json
import os
import re
import shutil
import struct
import sys
import zlib

DEFAULT_DIRS = [r"D:\EpicGames"]
DEFAULT_MANIFESTS_DIR = r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests"
DEFAULT_INSTALLED_DAT = r"C:\ProgramData\Epic\UnrealEngineLauncher\LauncherInstalled.dat"
DEFAULT_CATCACHE = r"C:\ProgramData\Epic\EpicGamesLauncher\Data\Catalog\catcache.bin"
# Base d'installations d'EOSH (Epic Online Services Host) : c'est ELLE que le
# launcher interroge au démarrage ("EOSH reconciliation"). Sans fiche .egi ici,
# le launcher vide puis supprime les .item, même valides.
DEFAULT_EGI_DIR = r"C:\ProgramData\Epic\EpicOnlineServicesShared\InstallHelper\InstalledItems"

DM_MAGIC = 0x44BEC00C

# Champs persistants extraits de chaque jeu (survit à la suppression des
# .mancpn par le launcher) : petite base JSON stockée à côté du script.
DEFAULT_DB = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                          "epic_relink_db.json")
DB_FIELDS = ("app_name", "version", "launch_exe", "catalog_ns",
             "catalog_item", "display_name")


def load_db(path):
    if os.path.isfile(path):
        try:
            return json.load(open(path, encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            pass
    return {}


def save_db(path, db):
    try:
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(db, f, indent=1, ensure_ascii=False)
        os.replace(tmp, path)  # écriture atomique
    except OSError:
        pass


# ---------------------------------------------------------------------------
# Lecture du manifest binaire Epic (.egstore/*.manifest)
# ---------------------------------------------------------------------------

def sha1_file(path):
    h = hashlib.sha1()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def meta_strings(manifest_path):
    """Extrait les strings du bloc meta : [AppName, Version, LaunchExecutable, ...]"""
    data = open(manifest_path, "rb").read()
    if len(data) < 42 or struct.unpack_from("<i", data, 0)[0] != DM_MAGIC:
        return []
    # offset 41 = début du stream zlib (header 16o + sha1 20o + flag 1o + int32 4o)
    try:
        out = zlib.decompressobj().decompress(data[41:])
    except zlib.error:
        return []
    strings, off = [], 14  # meta u32 + flag u8 + comp u32 + 5 octets inconnus
    for _ in range(8):
        if off + 4 > len(out):
            break
        n = struct.unpack_from("<I", out, off)[0]
        if n == 0 or n > 512 or off + 4 + n > len(out):
            break
        raw = out[off + 4:off + 4 + n - 1]
        if not raw or any(c < 0x20 or c > 0x7E for c in raw):
            break
        strings.append(raw.decode("ascii"))
        off += 4 + n
    return strings


def scan_game(game_dir):
    """Collecte toutes les infos récupérables depuis le dossier d'un jeu."""
    egstore = os.path.join(game_dir, ".egstore")
    if not os.path.isdir(egstore):
        return None

    manifests = [f for f in os.listdir(egstore)
                 if f.lower().endswith(".manifest") and len(f) >= 40]
    mancpns = [f for f in os.listdir(egstore) if f.lower().endswith(".mancpn")]
    if not manifests:
        return None

    mpath = os.path.join(egstore, manifests[0])
    guid = manifests[0][:-len(".manifest")].upper()

    info = {
        "guid": guid,
        "manifest_path": mpath,
        "manifest_hash": sha1_file(mpath),
        "app_name": "",
        "version": "",
        "launch_exe": "",
        "catalog_ns": "",
        "catalog_item": "",
        "display_name": "",
        "install_size": 0,
    }

    strs = meta_strings(mpath)
    if len(strs) >= 3:
        info["app_name"], info["version"], info["launch_exe"] = strs[0], strs[1], strs[2]

    if mancpns:
        try:
            mc = json.load(open(os.path.join(egstore, mancpns[0]), encoding="utf-8"))
            # le mancpn fait foi pour les IDs catalogue
            info["catalog_ns"] = mc.get("CatalogNamespace", info["catalog_ns"])
            info["catalog_item"] = mc.get("CatalogItemId", info["catalog_item"])
            if mc.get("AppName"):
                info["app_name"] = mc["AppName"]
        except (json.JSONDecodeError, OSError):
            pass

    # taille d'installation = somme des fichiers, hors .egstore (calibré sur le
    # launcher officiel : la valeur .item correspond exactement à cette somme)
    total = 0
    for root, _dirs, files in os.walk(game_dir):
        if ".egstore" in root.lower():
            continue
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    info["install_size"] = total
    return info


# ---------------------------------------------------------------------------
# Cache catalogue (DisplayName)
# ---------------------------------------------------------------------------

def load_catalog_titles(catcache_path):
    """Indexe le cache catalogue du launcher : {(namespace, item_id): title}."""
    if not os.path.isfile(catcache_path):
        return {}
    try:
        raw = open(catcache_path, "rb").read()
        cache = json.loads(base64.b64decode(raw))
        return {(e.get("namespace"), e.get("id")): e.get("title", "")
                for e in cache if isinstance(e, dict)}
    except Exception:
        return {}


def guess_display_name(info, game_dir):
    if info["launch_exe"]:
        name = re.sub(r"\.exe$", "", info["launch_exe"], flags=re.I)
        if name and not re.match(r"^(unins|setup|UnityCrashHandler|EOSBootstrapper)",
                                 name, re.I):
            return name
    return os.path.basename(game_dir)


# ---------------------------------------------------------------------------
# Génération / mise à jour du .item
# ---------------------------------------------------------------------------

def win(path):
    return path.replace("/", "\\")


def build_item(info, game_dir, existing=None):
    """Construit le dict .item (en se basant sur l'existant si fourni)."""
    egstore = win(os.path.join(game_dir, ".egstore"))
    guid = info["guid"]
    item = dict(existing) if existing else {
        "FormatVersion": 0,
        "EoshRevision": "00000000000000000000000000000000",
        "bIsIncompleteInstall": False,
        "LaunchCommand": "",
        "LaunchExecutable": "",
        "ManifestLocation": "",
        "CompleteManifestPath": "",
        "PendingManifestPath": "",
        "ManifestHash": "",
        "SDMetaHash": "",
        "SDMetaLocation": "",
        "bIsApplication": True,
        "bIsExecutable": True,
        "bIsManaged": False,
        "bIsFab": False,
        "bNeedsValidation": False,
        "bSDMetaMigrated": False,
        "bRequiresAuth": True,
        "bAllowMultipleInstances": False,
        "bCanRunOffline": False,
        "bAllowUriCmdArgs": False,
        "bLaunchElevated": False,
        # Les URLs de download ne sont stockées nulle part localement ; le
        # launcher les re-télécharge depuis l'API quand il en a besoin.
        "BaseURLs": [],
        "BuildLabel": "Live",
        "AppCategories": ["games", "applications"],
        "ChunkDbs": [],
        "CompatibleApps": [],
        "DisplayName": "",
        "InstallationGuid": guid,
        "InstallLocation": "",
        "InstallSessionId": "00000000000000000000000000000000",
        "InstallTags": [],
        "InstallComponents": [],
        "HostInstallationGuid": "00000000000000000000000000000000",
        "PrereqSHA1Hash": "",
        "LastPrereqSucceededSHA1Hash": "",
        "StagingLocation": "",
        "TechnicalType": "games,applications",
        "VaultThumbnailUrl": "",
        "VaultTitleText": "",
        "InstallSize": 0,
        "MainWindowProcessName": "",
        "ProcessNames": [],
        "BackgroundProcessNames": [],
        "IgnoredProcessNames": [],
        "DlcProcessNames": [],
        "MandatoryAppFolderName": "",
        "OwnershipToken": "false",
        "SidecarConfigRevision": 1,
        "SidecarDeploymentId": "",
        "PreloadState": 0,
        "CatalogNamespace": "",
        "CatalogItemId": "",
        "AppName": "",
        "AppVersionString": "",
        "MainGameCatalogNamespace": "",
        "MainGameCatalogItemId": "",
        "MainGameAppName": "",
        "AllowedUriEnvVars": [],
    }

    item.update({
        "LaunchExecutable": info["launch_exe"],
        "ManifestLocation": egstore,
        "CompleteManifestPath": egstore + "\\" + guid + ".manifest",
        "PendingManifestPath": egstore + "\\Pending\\" + guid + ".manifest",
        "ManifestHash": info["manifest_hash"],
        "DisplayName": info["display_name"],
        "InstallLocation": win(game_dir),
        "StagingLocation": egstore + "/bps",
        "InstallSize": info["install_size"],
        "MandatoryAppFolderName": os.path.basename(game_dir),
        "CatalogNamespace": info["catalog_ns"],
        "CatalogItemId": info["catalog_item"],
        "AppName": info["app_name"],
        "AppVersionString": info["version"],
    })
    return item


def item_needs_update(item, info, game_dir):
    """True si le .item ne correspond plus au disque ou a été vidé/dégradé."""
    loc = win(game_dir)
    checks = (
        item.get("InstallLocation") == loc,
        item.get("ManifestHash") == info["manifest_hash"],
        item.get("AppVersionString") == info["version"],
        item.get("InstallSize") == info["install_size"],
        item.get("CatalogItemId") == info["catalog_item"],
        item.get("CatalogNamespace") == info["catalog_ns"],
        item.get("LaunchExecutable") == info["launch_exe"],
        # un .item vidé par la réconciliation EOSH a ces champs vides :
        bool(item.get("CatalogItemId")),
        bool(item.get("LaunchExecutable")),
    )
    return not all(checks)


def write_item(path, item):
    with open(path, "w", encoding="utf-8", newline="\r\n") as f:
        f.write(json.dumps(item, indent="\t", ensure_ascii=False))


# ---------------------------------------------------------------------------
# LauncherInstalled.dat (UTF-16LE sans BOM)
# ---------------------------------------------------------------------------

def load_installed_dat(path):
    """Charge LauncherInstalled.dat en détectant l'encodage.

    Le launcher a écrit ce fichier en UTF-16LE sur certaines versions et en
    ASCII/UTF-8 sur d'autres ; on détecte et on conserve l'existant.
    """
    raw = open(path, "rb").read()
    if raw[:2] in (b"\xff\xfe", b"\xfe\xff"):
        enc = "utf-16"  # BOM présent
    elif raw[:4].count(0) >= 2 or raw[200:300].count(0) > 10:
        enc = "utf-16-le"
    else:
        enc = "ascii"
    return json.loads(raw.decode(enc)), enc


def save_installed_dat(path, data, enc):
    with open(path, "wb") as f:
        f.write(json.dumps(data, indent="\t", ensure_ascii=False).encode(enc))


# ---------------------------------------------------------------------------
# Fiches d'installation EOSH (.egi) + révision
# ---------------------------------------------------------------------------

def dotnet_ticks(dt):
    """Ticks .NET (100 ns depuis 0001-01-01) pour un datetime UTC."""
    from datetime import datetime
    return int((dt - datetime(1, 1, 1)).total_seconds() * 10_000_000)


def build_revision(timestamp, number):
    """La 'revision' = %016X du timestamp .NET concaténé au numéro.

    Vérifié : Revision.json {timestamp:639158058319734218, number:5} ->
    le launcher reçoit '08DEBEDE1D79E5CA0000000000000005'.
    """
    return "%016X%016X" % (timestamp, number)


def read_revision(egi_dir):
    """Lit Revision.json ; retourne (timestamp, number) ou défaut."""
    path = os.path.join(egi_dir, "Revision.json")
    if os.path.isfile(path):
        try:
            r = json.load(open(path, encoding="utf-8"))
            return int(r.get("timestamp", 0)), int(r.get("number", 0))
        except (json.JSONDecodeError, ValueError, OSError):
            pass
    return 0, 0


def build_egi(info, game_dir, revision):
    """Construit le dict de la fiche EOSH (format 'v4' observé sur Brotato)."""
    g = info["guid"]
    loc = win(game_dir)
    empty_data = {"version": "", "launchCommand": "", "launchExe": "",
                  "prereqSHA1Hash": "", "buildSize": 0}
    return {"v4": {
        "installationId": g,
        "state": "Installed",
        "revision": revision,
        "dir": loc,
        "metaDir": loc + "\\.egstore",
        "manifestPath": loc + "\\.egstore\\" + g + ".manifest",
        "pendingManifestPath": loc + "\\.egstore\\Pending\\" + g + ".manifest",
        "platform": "Windows",
        "sandboxId": info["catalog_ns"],
        "itemId": info["catalog_item"],
        "artifactId": info["app_name"],
        "tags": [],
        "pendingTags": [],
        "stagedVersion": "",
        "manifestData": {
            "version": info["version"],
            "launchCommand": "",
            "launchExe": info["launch_exe"],
            "prereqSHA1Hash": "",
            "buildSize": info["install_size"],
        },
        "pendingManifestData": dict(empty_data),
    }}


def write_egi(egi_dir, info, game_dir, revision, dry_run):
    """Écrit la fiche .egi si absente ou différente ; True si changement."""
    path = os.path.join(egi_dir, info["guid"] + ".egi")
    content = json.dumps(build_egi(info, game_dir, revision),
                         separators=(",", ":"), ensure_ascii=False)
    if os.path.isfile(path):
        try:
            if open(path, encoding="utf-8").read() == content:
                return False  # déjà à jour
        except OSError:
            pass
        if not dry_run:
            shutil.copyfile(path, path + ".bak")
    if not dry_run:
        os.makedirs(egi_dir, exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(content)
    return True


def bump_revision(egi_dir, dry_run):
    """Incrémente Revision.json ; retourne la nouvelle chaîne de révision."""
    ts, num = read_revision(egi_dir)
    from datetime import datetime, timezone
    new_ts = dotnet_ticks(datetime.now(timezone.utc).replace(tzinfo=None))
    if new_ts <= ts:
        new_ts = ts + 1  # la révision doit toujours croître
    new_num = num + 1
    if not dry_run:
        path = os.path.join(egi_dir, "Revision.json")
        if os.path.isfile(path):
            shutil.copyfile(path, path + ".bak")
        os.makedirs(egi_dir, exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(json.dumps({"timestamp": new_ts, "number": new_num},
                               separators=(",", ":")))
    return build_revision(new_ts, new_num)


def write_mancpn(game_dir, info, dry_run):
    """Restaure le .mancpn du jeu (supprimé par le launcher lors d'un rejet EOSH).

    Rend le dossier .egstore à nouveau autonome ; idempotent.
    """
    if not (info["catalog_ns"] and info["catalog_item"] and info["app_name"]):
        return False
    path = os.path.join(game_dir, ".egstore", info["guid"] + ".mancpn")
    content = json.dumps({"FormatVersion": 0,
                          "CatalogNamespace": info["catalog_ns"],
                          "CatalogItemId": info["catalog_item"],
                          "AppName": info["app_name"]},
                         separators=(",", ":"))
    if os.path.isfile(path):
        try:
            if json.load(open(path, encoding="utf-8")) == json.loads(content):
                return False
        except (json.JSONDecodeError, OSError):
            pass
        if not dry_run:
            shutil.copyfile(path, path + ".bak")
    if not dry_run:
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(content)
    return True


def upsert_installation(dat, info, game_dir):
    """Ajoute ou met à jour l'entrée du jeu ; True si modification."""
    entries = dat.setdefault("InstallationList", [])
    loc = win(game_dir)

    def merge(e):
        """Met à jour l'entrée sans jamais écraser une valeur par du vide."""
        changed = False
        for key, val in (("InstallLocation", loc),
                         ("NamespaceId", info["catalog_ns"]),
                         ("ItemId", info["catalog_item"]),
                         ("ArtifactId", info["app_name"]),
                         ("AppVersion", info["version"]),
                         ("AppName", info["app_name"])):
            if val and e.get(key) != val:
                e[key] = val
                changed = True
        return changed

    for e in entries:
        if e.get("AppName") == info["app_name"] or e.get("ArtifactId") == info["app_name"]:
            return merge(e)
    entries.append({
        "InstallLocation": loc,
        "NamespaceId": info["catalog_ns"],
        "ItemId": info["catalog_item"],
        "ArtifactId": info["app_name"],
        "AppVersion": info["version"],
        "AppName": info["app_name"],
    })
    return True


# ---------------------------------------------------------------------------
# Programme principal
# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="Régénère les .item Epic Games pour des jeux déjà installés.")
    ap.add_argument("--dirs", nargs="+", default=DEFAULT_DIRS,
                    help="Répertoires contenant les jeux (défaut: D:\\EpicGames)")
    ap.add_argument("--manifests-dir", default=DEFAULT_MANIFESTS_DIR,
                    help="Dossier des .item du launcher")
    ap.add_argument("--installed-dat", default=DEFAULT_INSTALLED_DAT,
                    help="Chemin de LauncherInstalled.dat")
    ap.add_argument("--catcache", default=DEFAULT_CATCACHE,
                    help="Cache catalogue (pour les noms de jeux)")
    ap.add_argument("--egi-dir", default=DEFAULT_EGI_DIR,
                    help="Dossier des fiches d'installation EOSH (.egi)")
    ap.add_argument("--db", default=DEFAULT_DB,
                    help="Base locale des métadonnées de jeux (JSON)")
    ap.add_argument("--dry-run", action="store_true", help="Affiche sans modifier")
    ap.add_argument("--force", action="store_true", help="Régénère même les .item à jour")
    args = ap.parse_args()

    titles = load_catalog_titles(args.catcache)
    db = load_db(args.db)

    # index des .item existants (par GUID et par AppName)
    existing_by_guid, existing_by_app = {}, {}
    if os.path.isdir(args.manifests_dir):
        for f in os.listdir(args.manifests_dir):
            if not f.lower().endswith(".item"):
                continue
            try:
                it = json.load(open(os.path.join(args.manifests_dir, f), encoding="utf-8"))
            except (json.JSONDecodeError, OSError):
                continue
            existing_by_guid[f[:-5].upper()] = it
            if it.get("AppName"):
                existing_by_app[it["AppName"]] = it

    dat = None
    dat_enc = "ascii"
    dat_changed = False
    if os.path.isfile(args.installed_dat):
        try:
            dat, dat_enc = load_installed_dat(args.installed_dat)
        except (json.JSONDecodeError, UnicodeError) as e:
            print(f"!! LauncherInstalled.dat illisible ({e}), section ignorée", file=sys.stderr)

    # lookup AppName -> (NamespaceId, ItemId) depuis LauncherInstalled.dat
    dat_lookup = {}
    if dat is not None:
        for e in dat.get("InstallationList", []):
            if e.get("ArtifactId"):
                dat_lookup[e["ArtifactId"]] = (e.get("NamespaceId", ""),
                                               e.get("ItemId", ""))

    # ------------------------------------------------------------------
    # Phase 1 : scan des jeux
    # ------------------------------------------------------------------
    jobs = []  # (game_dir, info, item_existant, item_path)
    for base in args.dirs:
        if not os.path.isdir(base):
            print(f"!! Répertoire introuvable : {base}", file=sys.stderr)
            continue
        for entry in sorted(os.listdir(base)):
            game_dir = os.path.join(base, entry)
            if not os.path.isdir(game_dir):
                continue
            info = scan_game(game_dir)
            if info is None:
                print(f"-- {entry} : pas de .egstore/manifest, ignoré")
                continue

            # compléter les métadonnées perdues (le launcher supprime parfois
            # les .mancpn) : 1) base locale, 2) LauncherInstalled.dat
            known = db.get(info["guid"], {})
            for k in DB_FIELDS:
                if not info.get(k) and known.get(k):
                    info[k] = known[k]
            if not info["catalog_item"] and info["app_name"] in dat_lookup:
                ns, item_id = dat_lookup[info["app_name"]]
                info["catalog_ns"] = info["catalog_ns"] or ns
                info["catalog_item"] = info["catalog_item"] or item_id

            title = titles.get((info["catalog_ns"], info["catalog_item"]), "")
            info["display_name"] = title or info["display_name"] \
                or guess_display_name(info, game_dir)
            if not info["catalog_item"]:
                print(f"!! {entry} : IDs catalogue inconnus "
                      f"(mancpn perdu et base vide)", file=sys.stderr)

            # mémoriser pour la prochaine fois : fusion ADDITIVE par champ —
            # on n'ajoute/actualise que des valeurs non vides, on ne supprime
            # jamais une clé ni une entrée (même si le jeu a disparu du disque)
            entry = db.setdefault(info["guid"], {})
            for k in DB_FIELDS:
                if info.get(k):
                    entry[k] = info[k]

            item = existing_by_guid.get(info["guid"]) or existing_by_app.get(info["app_name"])
            item_path = os.path.join(args.manifests_dir, info["guid"] + ".item")
            jobs.append((game_dir, info, item, item_path))

    save_db(args.db, db)

    # ------------------------------------------------------------------
    # Phase 2 : EOSH — une seule révision pour tout le lot
    # ------------------------------------------------------------------
    ts, num = read_revision(args.egi_dir)
    cur_revision = build_revision(ts, num)
    egi_would_change = any(
        write_egi(args.egi_dir, info, gd, cur_revision, dry_run=True)
        for gd, info, _it, _p in jobs)
    if egi_would_change:
        revision = bump_revision(args.egi_dir, args.dry_run)  # notifie le launcher
        print(f"Révision EOSH : {cur_revision} -> {revision}" +
              (" (simulation)" if args.dry_run else ""))
    else:
        revision = cur_revision

    # ------------------------------------------------------------------
    # Phase 3 : écriture .item + .egi + LauncherInstalled.dat
    # ------------------------------------------------------------------
    actions = {"created": [], "rellinked": [], "updated": [], "skipped": [], "failed": []}

    for game_dir, info, item, item_path in jobs:
        try:
            if item is not None and not args.force:
                if item_needs_update(item, info, game_dir):
                    new_item = build_item(info, game_dir, existing=item)
                    new_item["EoshRevision"] = revision
                    actions["rellinked"].append(info["display_name"])
                    if not args.dry_run:
                        shutil.copyfile(item_path, item_path + ".bak")
                        write_item(item_path, new_item)
                else:
                    actions["skipped"].append(info["display_name"])
            else:
                new_item = build_item(info, game_dir, existing=item)
                new_item["EoshRevision"] = revision
                actions["created" if item is None else "updated"].append(info["display_name"])
                if not args.dry_run:
                    if os.path.isfile(item_path):
                        shutil.copyfile(item_path, item_path + ".bak")
                    write_item(item_path, new_item)

            write_egi(args.egi_dir, info, game_dir, revision, args.dry_run)
            write_mancpn(game_dir, info, args.dry_run)

            if dat is not None and upsert_installation(dat, info, game_dir):
                dat_changed = True
        except OSError as e:
            actions["failed"].append(f"{info['display_name']} ({e})")

    if dat is not None and dat_changed and not args.dry_run:
        shutil.copyfile(args.installed_dat, args.installed_dat + ".bak")
        save_installed_dat(args.installed_dat, dat, dat_enc)

    print()
    print(f"[{ 'SIMULATION' if args.dry_run else 'OK' }] "
          f"{len(actions['created'])} créé(s), {len(actions['updated'])} régénéré(s), "
          f"{len(actions['rellinked'])} relinké(s), {len(actions['skipped'])} déjà à jour, "
          f"{len(actions['failed'])} en échec")
    for k in ("created", "updated", "rellinked", "skipped", "failed"):
        for name in actions[k]:
            print(f"  {k:9s} : {name}")
    if dat_changed:
        print("  LauncherInstalled.dat : mis à jour" + (" (simulation)" if args.dry_run else ""))
    print("\nPensez à redémarrer le launcher Epic Games.")


if __name__ == "__main__":
    main()
