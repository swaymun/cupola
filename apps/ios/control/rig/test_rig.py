#!/usr/bin/env python3
"""Offline safety/input contracts. No simulator boots or HID libraries load."""
import contextlib
import importlib.machinery
import io
import os
from pathlib import Path
import tempfile
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[4]
loader = importlib.machinery.SourceFileLoader('wonderctl_rig', str(ROOT / 'scripts/wonderctl'))
w = ModuleType(loader.name)
w.__file__ = loader.path
loader.exec_module(w)


class RigTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        derived = root / 'derived'
        app = derived / 'Build/Products/Diagnostics-iphonesimulator/Wonder.app'
        app.mkdir(parents=True)
        # cmd_run hashes the app even when installation is stubbed. Keep that
        # real runner path, using a synthetic binary and disposable receipts
        # rather than depending on (or writing into) a built simulator lane.
        (app / 'Wonder').write_bytes(b'offline rig routing fixture')
        for context in (patch.object(w, 'DERIVED', derived),
                        patch.object(w, 'EVIDENCE', root / 'evidence')):
            context.start()
            self.addCleanup(context.stop)

    inventory = '''Unique ID: inner
        Device Name: primary-1
        Seed: 98
        Screen Type: Integrated
        Pixel Size: {2007, 2853}
        Dot Pitch: 460
        Preferred UI Scale: 3
        UI Orientation: Portrait
        Unique ID: cover
        Device Name: primary
        Screen Type: Integrated
        Pixel Size: {1398, 2034}
        Dot Pitch: 460
        Preferred UI Scale: 3
        UI Orientation: Portrait
    '''

    def test_portrait_cover_selected(self):
        self.assertEqual(w.rig_display(self.inventory,466,678), ('cover', 'Portrait', (466,678), 'primary'))

    def test_unqualified_orientation_or_stale_bounds_posts_nothing(self):
        inventories = [self.inventory.replace('Portrait', value) for value in ('Landscape Left', 'Landscape Right', 'Portrait Upside Down', 'Ambiguous')]
        inventories.append(self.inventory.replace('UI Orientation: Portrait', ''))
        cases = [(inventory, (466,678)) for inventory in inventories]
        cases += [(self.inventory, size) for size in ((678,466), (669,951), (951,669))]
        device = {'state':'Booted', 'deviceTypeIdentifier':'com.apple.CoreSimulator.SimDeviceType.iPhone-Duo', 'udid':'test'}
        for inventory, size in cases:
            with self.subTest(size=size, inventory=inventory), patch.object(w, 'find_device', return_value=device), patch.object(w, 'simctl', return_value=SimpleNamespace(stdout=inventory)) as simctl, patch.object(w, 'sh') as command:
                with self.assertRaises(w.ControlError):
                    w.simrig('tap', 20, 30, *size)
                simctl.assert_called_once_with('io', 'test', 'enumerate')
                command.assert_not_called()

    def test_measured_inner_targets_map_to_the_natural_digitizer(self):
        # Points measured from native Settings and Swap targets, not generated
        # from the mapping under test. The cover retains its original contract.
        cases = [('Portrait', (24,61,466,678), (24,61,466,678,'cover',0)),
                 ('Portrait Upside Down', (143,104,669,951), (526,847,669,951,'inner',1)),
                 ('Landscape Left', (230,512,951,669), (512,721,669,951,'inner',1))]
        for orientation, point, expected in cases:
            inventory = self.inventory.replace('UI Orientation: Portrait', 'UI Orientation: '+orientation, 1)
            with self.subTest(orientation=orientation):
                self.assertEqual(w.rig_tap(inventory,*point),expected)
        inventory = self.inventory.replace('UI Orientation: Portrait', 'UI Orientation: Portrait Upside Down', 1)
        x,y,width,height,*_ = w.rig_tap(inventory,0,0,669,951)
        self.assertLess(x,width)
        self.assertLess(y,height)
        for point in [(-1,0), (669,0), (0,951), (float('nan'),0)]:
            with self.subTest(point=point), self.assertRaises(w.ControlError):
                w.rig_tap(inventory,*point,669,951)

    def test_ambiguous_or_missing_display_refuses_input(self):
        for inventory, size in [(self.inventory,(500,500)), (self.inventory+self.inventory,(466,678))]:
            with self.subTest(size=size), self.assertRaises(w.ControlError):
                w.rig_display(inventory,*size)

    def test_refuses_missing_shutdown_or_physical_target_before_any_command(self):
        for device in (None, {'state': 'Shutdown'}, {'state': 'Booted', 'deviceTypeIdentifier': 'iPhone17,3'}):
            with self.subTest(device=device), patch.object(w, 'find_device', return_value=device), patch.object(w, 'sh') as command:
                with self.assertRaises(w.ControlError):
                    w.simrig('keys', '4')
                command.assert_not_called()

    def test_hinge_and_orientation_refuse_other_simulator_types(self):
        device = {'state': 'Booted', 'deviceTypeIdentifier': 'com.apple.CoreSimulator.SimDeviceType.iPad-Pro'}
        for action in ('hinge', 'orient'):
            with self.subTest(action=action), patch.object(w, 'find_device', return_value=device), patch.object(w, 'sh') as command:
                with self.assertRaises(w.ControlError):
                    w.simrig(action, '0')
                command.assert_not_called()

    def test_coordinate_tap_with_explicit_size_does_not_open_accessibility(self):
        args = w.parser().parse_args(['tap', '--xy', '24,61', '--size', '466,678'])
        with patch.dict(os.environ, WONDER_CONTROL_INPUT='rig'), patch.object(w, 'ui_tree') as tree, patch.object(w, 'axe') as axe, patch.object(w, 'simrig') as rig, contextlib.redirect_stdout(io.StringIO()):
            w.cmd_tap(args)
        tree.assert_not_called()
        axe.assert_not_called()
        rig.assert_called_once_with('tap', '24', '61', '466', '678')

    def test_selector_tap_reuses_one_tree_and_ignores_hidden_copy(self):
        args = w.parser().parse_args(['tap', '--id', 'save'])
        tree = [{'type': 'Application', 'frame': {'x':0,'y':0,'width':466,'height':678}, 'children': [
            {'AXUniqueId':'save','frame':{'x':-100,'y':20,'width':40,'height':40}},
            {'AXUniqueId':'save','frame':{'x':100,'y':20,'width':40,'height':40}}]}]
        with patch.dict(os.environ, WONDER_CONTROL_INPUT='rig'), patch.object(w, 'ui_tree', return_value=tree) as read, patch.object(w, 'simrig') as rig, contextlib.redirect_stdout(io.StringIO()):
            w.cmd_tap(args)
        read.assert_called_once()
        rig.assert_called_once_with('tap', '120', '40', 466, 678)

    def test_offscreen_taps_are_refused_without_edge_retargeting(self):
        for point in ('200,800', '-1,30', '466,30', '20,678', '20,-1'):
            args = w.parser().parse_args(['tap', f'--xy={point}', '--size', '466,678'])
            with self.subTest(point=point), patch.dict(os.environ, WONDER_CONTROL_INPUT='rig'), patch.object(w, 'ui_tree') as tree, patch.object(w, 'simrig') as rig:
                with self.assertRaises(w.ControlError):
                    w.cmd_tap(args)
                tree.assert_not_called()
                rig.assert_not_called()

    def test_ad_hoc_input_defaults_to_axe_with_explicit_overrides(self):
        for profile in ('duo', 'ipad', 'iphone', 'iphone-27'):
            for override in (None, 'axe', 'rig'):
                env = {'WONDER_CONTROL_PROFILE': profile}
                if override is not None:
                    env['WONDER_CONTROL_INPUT'] = override
                with self.subTest(profile=profile, override=override), patch.dict(os.environ, env, clear=True):
                    self.assertEqual(w.rig_input(), override == 'rig')

    def test_qualified_scenario_routes_taps_only_on_duo_and_honors_overrides(self):
        for filename in ('agent-thread-messages.wctl', 'voice-mode.wctl'):
            script = ROOT / 'apps/ios/control/scenarios' / filename
            for profile in ('duo', 'ipad', 'iphone', 'iphone-27'):
                for inherited, override in ((None, None), ('axe', None), ('rig', None), ('rig', 'axe'), ('axe', 'rig')):
                    env = {'WONDER_CONTROL_PROFILE': profile}
                    if inherited is not None:
                        env['WONDER_CONTROL_INPUT'] = inherited
                    with self.subTest(filename=filename, profile=profile, inherited=inherited, override=override), patch.dict(os.environ, env, clear=True), patch.object(w, 'simrig') as rig, patch.object(w, 'axe') as axe, patch.object(w, 'udid', return_value='sim'), contextlib.redirect_stdout(io.StringIO()):
                        with w.scenario_input(script, override):
                            w.tap_at(24, 61, size=(466,678))
                        if (override or inherited or ('rig' if profile == 'duo' else 'axe')) == 'rig':
                            rig.assert_called_once_with('tap', 24, 61, 466, 678)
                            axe.assert_not_called()
                        else:
                            axe.assert_called_once_with(['tap', '-x', '24', '-y', '61', '--udid', 'sim'])
                            rig.assert_not_called()
                        self.assertEqual(os.environ.get('WONDER_CONTROL_INPUT'), inherited)

    def test_unqualified_canonical_scenario_keeps_axe_offscreen_behavior(self):
        script = ROOT / 'apps/ios/control/scenarios/claude-access-picker.wctl'
        with patch.dict(os.environ, {'WONDER_CONTROL_PROFILE':'duo'}, clear=True), patch.object(w, 'simrig') as rig, patch.object(w, 'axe') as axe, patch.object(w, 'udid', return_value='sim'), contextlib.redirect_stdout(io.StringIO()):
            with w.scenario_input(script):
                w.tap_at(200, 800, size=(466,678))
            axe.assert_called_once_with(['tap', '-x', '200', '-y', '800', '--udid', 'sim'])
            rig.assert_not_called()

    def test_copied_qualified_filename_stays_on_axe(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / 'agent-thread-messages.wctl'
            script.write_text('tap --xy 24,61 --size 466,678\n')
            args = w.parser().parse_args(['run', str(script)])
            with patch.dict(os.environ, {'WONDER_CONTROL_PROFILE':'duo'}, clear=True), patch.object(w, 'ensure_own_build'), patch.object(w, 'udid', return_value='sim'), patch.object(w, 'axe') as axe, patch.object(w, 'simrig') as rig, contextlib.redirect_stdout(io.StringIO()):
                w.cmd_run(args)
                axe.assert_called_once()
                rig.assert_not_called()
                self.assertNotIn('WONDER_CONTROL_INPUT', os.environ)

    def test_scenario_flag_selects_rig_and_restores_backend_after_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / 'unqualified.wctl'
            script.write_text('tap --xy 24,61 --size 466,678\n')
            args = w.parser().parse_args(['run', str(script), '--input', 'rig'])
            with patch.dict(os.environ, {'WONDER_CONTROL_PROFILE':'duo', 'WONDER_CONTROL_INPUT':'axe'}, clear=True), patch.object(w, 'ensure_own_build'), patch.object(w, 'udid', return_value='sim'), patch.object(w, 'axe') as axe, patch.object(w, 'simrig', side_effect=w.ControlError('rig failed')) as rig, patch.object(w, 'capture_failure', side_effect=w.ControlError('offline')), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(w.ControlError, 'rig failed'):
                    w.cmd_run(args)
                rig.assert_called_once_with('tap', '24', '61', '466', '678')
                axe.assert_not_called()
                self.assertEqual(os.environ['WONDER_CONTROL_INPUT'], 'axe')

    def test_changed_compiler_flags_get_a_new_binary_and_command(self):
        source = ROOT / 'apps/ios/control/rig'
        digest, binary, command = w.rig_build_plan(source)
        with patch.object(w, 'RIG_COMPILE_FLAGS', (*w.RIG_COMPILE_FLAGS, '-g')):
            changed_digest, changed_binary, changed_command = w.rig_build_plan(source)
        self.assertNotEqual(digest, changed_digest)
        self.assertNotEqual(binary, changed_binary)
        self.assertNotIn('-g', command)
        self.assertIn('-g', changed_command)
        self.assertEqual(changed_command[changed_command.index('-o')+1], str(changed_binary))

    def test_nonfinite_coordinate_posts_nothing(self):
        with patch.dict(os.environ, WONDER_CONTROL_INPUT='rig'), patch.object(w, 'simrig') as rig:
            with self.assertRaises(w.ControlError):
                w.tap_at('nan', 20, size=(466,678))
        rig.assert_not_called()

    def test_type_sends_us_keyboard_usages_and_balanced_shift(self):
        with patch.dict(os.environ, WONDER_CONTROL_INPUT='rig'), patch.object(w, 'simrig') as rig, contextlib.redirect_stdout(io.StringIO()):
            w.cmd_type(SimpleNamespace(text='aZ/@[]\n'))
        rig.assert_called_once_with('keys', '4,e1:down,1d,e1:up,38,e1:down,1f,e1:up,2f,30,28')

    def test_unsupported_text_posts_nothing(self):
        with patch.dict(os.environ, WONDER_CONTROL_INPUT='rig'), patch.object(w, 'simrig') as rig:
            with self.assertRaises(w.ControlError):
                w.cmd_type(SimpleNamespace(text='prefix🙂'))
        rig.assert_not_called()

    def test_unknown_backend_does_not_post_input(self):
        for backend in ('typo', ''):
            with self.subTest(backend=backend), patch.dict(os.environ, WONDER_CONTROL_INPUT=backend), patch.object(w, 'axe') as axe, patch.object(w, 'simrig') as rig:
                with self.assertRaises(w.ControlError):
                    w.tap_at(1, 2)
                with self.assertRaises(w.ControlError), w.scenario_input(ROOT / 'apps/ios/control/scenarios/agent-thread-messages.wctl'):
                    w.tap_at(1, 2)
                axe.assert_not_called()
                rig.assert_not_called()


if __name__ == '__main__':
    unittest.main()
