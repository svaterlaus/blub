#[macro_use]
extern crate more_asserts;
#[macro_use]
extern crate log;
#[macro_use]
extern crate strum_macros;
#[macro_use]
mod wgpu_utils;

mod camera;
mod global_bindings;
mod global_ubo;
mod gui;
mod render_output;
mod renderer;
mod scene;
mod simulation;
mod simulation_controller;
mod timer;
mod utils;
use render_output::{hdr_backbuffer::HdrBackbuffer, screen::Screen, screenshot_recorder::ScreenshotRecorder};
use renderer::SceneRenderer;
use simulation_controller::SimulationControllerStatus;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use wgpu_profiler::{GpuProfiler, GpuProfilerSettings};
use wgpu_utils::{pipelines, shader};
use winit::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::Window,
};

use global_bindings::*;
use global_ubo::*;

#[derive(Debug, Clone)]
pub enum ApplicationEvent {
    LoadScene(PathBuf),
    ResetScene,
    FastForwardSimulation(Duration),
    ResetAndStartRecording { recording_fps: f64 }, // to stop recording, pause the simulation controller.
    ChangePresentMode(wgpu::PresentMode),
}

struct Application {
    window: Window,
    window_id: winit::window::WindowId,
    window_surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    screen: Screen,
    hdr_backbuffer: HdrBackbuffer,
    screenshot_recorder: ScreenshotRecorder,

    device: wgpu::Device,
    command_queue: wgpu::Queue,

    profiler_rendering: GpuProfiler,
    profiler_simulation: GpuProfiler,

    shader_dir: shader::ShaderDirectory,
    pipeline_manager: pipelines::PipelineManager,
    scene: scene::Scene,
    scene_renderer: SceneRenderer,
    simulation_controller: simulation_controller::SimulationController,
    gui: gui::GUI,

    camera: camera::Camera,
    global_ubo: GlobalUBO,
    global_bindings: GlobalBindings,
}

impl Application {
    async fn new(event_loop: &EventLoop<ApplicationEvent>) -> Application {
        let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let window_attributes = Window::default_attributes()
            .with_title("Blub")
            .with_inner_size(winit::dpi::LogicalSize::new(1980, 1080));
        let window = event_loop
            .create_window(window_attributes)
            .expect("Failed to create window");
        let window_id = window.id();

        let window_surface = wgpu_instance
            .create_surface(&window)
            .expect("Failed to create surface");
        // `Surface` is tied to the window by handle; it is valid for the entire time we keep `window` alive.
        let window_surface: wgpu::Surface<'static> = unsafe { std::mem::transmute(window_surface) };

        let adapter = wgpu_instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&window_surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("Failed to find an appropriate adapter");

        let mut limits = adapter.limits().clone();
        limits.max_immediate_size = limits.max_immediate_size.max(256);

        let (device, command_queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("main device"),
                required_features: wgpu::Features::IMMEDIATES
                    | wgpu::Features::TEXTURE_BINDING_ARRAY
                    | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING
                    | wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
                    | wgpu::Features::CONSERVATIVE_RASTERIZATION
                    | wgpu::Features::CLEAR_TEXTURE
                    | GpuProfiler::ALL_WGPU_TIMER_FEATURES,
                required_limits: limits,
                ..Default::default()
            })
            .await
            .expect("Failed to create device");

        let shader_dir = shader::ShaderDirectory::new(Path::new("shader"), Path::new(".shadercache"));
        let mut pipeline_manager = pipelines::PipelineManager::new();

        let screen = Screen::new(
            &device,
            &window_surface,
            &adapter,
            Screen::DEFAULT_PRESENT_MODE,
            window.inner_size(),
            &shader_dir,
            &mut pipeline_manager,
        );
        let hdr_backbuffer = HdrBackbuffer::new(&device, screen.resolution(), &shader_dir, &mut pipeline_manager);
        let global_ubo = GlobalUBO::new(&device);
        let mut global_bindings = GlobalBindings::new(&device);
        let simulation_controller = simulation_controller::SimulationController::new();
        let mut scene_renderer = SceneRenderer::new(
            &device,
            &command_queue,
            &shader_dir,
            &mut pipeline_manager,
            global_bindings.bind_group_layout(),
            &hdr_backbuffer,
        );
        let gui = gui::GUI::new(&device, &window);

        let profiler_rendering = GpuProfiler::new(
            &device,
            GpuProfilerSettings {
                max_num_pending_frames: 4,
                ..Default::default()
            },
        )
        .expect("profiler (rendering)");
        let profiler_simulation = GpuProfiler::new(
            &device,
            GpuProfilerSettings {
                max_num_pending_frames: 16,
                ..Default::default()
            },
        )
        .expect("profiler (simulation)");

        // Load initial scene. Gui already needs to list all scenes, so we go there to grab the default selected.
        let scene = scene::Scene::new(
            gui.selected_scene(),
            &device,
            &command_queue,
            &shader_dir,
            &mut pipeline_manager,
            global_bindings.bind_group_layout(),
        )
        .unwrap();
        scene_renderer.on_new_scene(&device, &command_queue, &scene);
        global_bindings.create_bind_group(&device, &global_ubo, &scene.models);

        Application {
            window,
            window_id,
            window_surface,
            adapter,
            screen,
            hdr_backbuffer,
            screenshot_recorder: ScreenshotRecorder::new(),

            device,
            command_queue,

            profiler_rendering,
            profiler_simulation,

            shader_dir,
            pipeline_manager,
            scene,
            scene_renderer,
            simulation_controller,
            gui,

            camera: camera::Camera::new(),
            global_ubo,
            global_bindings,
        }
    }

    pub fn load_scene(&mut self, scene_path: &Path) {
        let new_scene = scene::Scene::new(
            scene_path,
            &self.device,
            &self.command_queue,
            &self.shader_dir,
            &mut self.pipeline_manager,
            self.global_bindings.bind_group_layout(),
        );

        match new_scene {
            Ok(scene) => {
                self.scene = scene;
                self.scene_renderer.on_new_scene(&self.device, &self.command_queue, &self.scene);
                self.global_bindings.create_bind_group(&self.device, &self.global_ubo, &self.scene.models);
            }
            Err(error) => {
                error!("Failed to load scene from {:?}: {:?}", scene_path, error);
            }
        }
    }

    fn run(mut self, event_loop: EventLoop<ApplicationEvent>) {
        let event_loop_proxy = event_loop.create_proxy();

        let _ = event_loop.run(move |event, elwt| {
            elwt.set_control_flow(ControlFlow::Poll);

            match &event {
                Event::UserEvent(event) => match event {
                    ApplicationEvent::LoadScene(scene_path) => {
                        self.load_scene(scene_path);
                        self.simulation_controller.restart();
                    }
                    ApplicationEvent::ResetScene => {
                        self.scene.reset(
                            &self.device,
                            &self.command_queue,
                            &self.shader_dir,
                            &mut self.pipeline_manager,
                            self.global_bindings.bind_group_layout(),
                        );
                        self.simulation_controller.restart();
                    }
                    ApplicationEvent::FastForwardSimulation(simulation_jump_length) => {
                        self.simulation_controller.fast_forward_steps(
                            *simulation_jump_length,
                            &self.device,
                            &self.command_queue,
                            &mut self.scene,
                            &self.pipeline_manager,
                            self.global_bindings.bind_group(), // values from last draw are good enough.
                        );
                    }
                    ApplicationEvent::ResetAndStartRecording { recording_fps } => {
                        self.scene.reset(
                            &self.device,
                            &self.command_queue,
                            &self.shader_dir,
                            &mut self.pipeline_manager,
                            self.global_bindings.bind_group_layout(),
                        );
                        self.simulation_controller.restart();
                        self.simulation_controller.start_recording_with_fixed_frame_length(*recording_fps);
                        self.screenshot_recorder.start_next_recording();
                    }
                    ApplicationEvent::ChangePresentMode(present_mode) => {
                        self.screen = Screen::new(
                            &self.device,
                            &self.window_surface,
                            &self.adapter,
                            *present_mode,
                            self.screen.resolution(),
                            &self.shader_dir,
                            &mut self.pipeline_manager,
                        );
                    }
                },
                Event::WindowEvent { window_id, event } if *window_id == self.window_id => {
                    let _ = self.gui.on_window_event(&self.window, event);

                    match event {
                        WindowEvent::CloseRequested => elwt.exit(),
                        WindowEvent::RedrawRequested => {
                            self.update();
                            self.draw(&event_loop_proxy);
                        }
                        WindowEvent::KeyboardInput {
                            event: ref key_evt,
                            is_synthetic: false,
                            ..
                        } => {
                            self.camera.on_window_event(event);

                            use winit::{
                                event::ElementState,
                                keyboard::{Key, NamedKey},
                            };
                            if key_evt.state == ElementState::Pressed {
                                match &key_evt.logical_key {
                                    Key::Named(NamedKey::Escape) => elwt.exit(),
                                    Key::Named(NamedKey::PrintScreen) => {
                                        self.screenshot_recorder.schedule_next_screenshot();
                                    }
                                    Key::Named(NamedKey::Space) => self.simulation_controller.pause_or_resume(),
                                    _ => {}
                                }
                            }
                        }
                        _ => {
                            self.camera.on_window_event(event);
                        }
                    }
                }
                Event::AboutToWait => {
                    self.window.request_redraw();
                }
                Event::DeviceEvent { event, .. } => {
                    self.camera.on_device_event(event);
                }
                Event::LoopExiting => {
                    // workaround for errors on shutdown while recording screenshots
                    self.screen.wait_for_pending_screenshots(&self.device);
                }
                _ => (),
            }
        });
    }

    fn window_resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        let present = self.screen.present_mode();
        self.screen = Screen::new(
            &self.device,
            &self.window_surface,
            &self.adapter,
            present,
            size,
            &self.shader_dir,
            &mut self.pipeline_manager,
        );
        self.hdr_backbuffer = HdrBackbuffer::new(&self.device, self.screen.resolution(), &self.shader_dir, &mut self.pipeline_manager);
        self.scene_renderer.on_window_resize(&self.device, &self.hdr_backbuffer);
    }

    fn update(&mut self) {
        // Shader/pipeline reload
        {
            let changed_files = self.shader_dir.drain_changed_files();
            if !changed_files.is_empty() {
                info!("detected shader changes. Reloading...");
                let timer = std::time::Instant::now();
                self.pipeline_manager.reload_changed(&self.device, &self.shader_dir, &changed_files);
                info!("shader reload took {:?}", std::time::Instant::now() - timer);
            }
        }

        self.camera.update(self.simulation_controller.timer());

        update_global_ubo(
            &mut self.global_ubo,
            &self.command_queue,
            self.camera.fill_global_uniform_buffer(self.screen.aspect_ratio()),
            self.simulation_controller.timer().fill_global_uniform_buffer(),
            self.scene_renderer.fill_global_uniform_buffer(&self.scene),
            self.screen.fill_global_uniform_buffer(),
        );
        self.simulation_controller.frame_steps(
            &mut self.scene,
            &self.device,
            &self.command_queue,
            &self.pipeline_manager,
            &mut self.profiler_simulation,
            self.global_bindings.bind_group(),
        );

        if self.simulation_controller.status() == SimulationControllerStatus::Paused {
            self.screenshot_recorder.stop_recording();
        }

        {
            let mut sim_settings = self.profiler_simulation.settings().clone();
            sim_settings.enable_timer_queries = self.gui.show_profiling_data_simulation();
            let _ = self.profiler_simulation.change_settings(sim_settings);
        }
        {
            let mut ren_settings = self.profiler_rendering.settings().clone();
            ren_settings.enable_timer_queries = self.gui.show_profiling_data_rendering();
            let _ = self.profiler_rendering.change_settings(ren_settings);
        }
        if let Some(profiling_data_rendering) = self.profiler_rendering.process_finished_frame(self.command_queue.get_timestamp_period()) {
            self.gui.report_profiling_data_rendering(profiling_data_rendering);
        }
        loop {
            if let Some(simulation_profiling_data) =
                self.profiler_simulation.process_finished_frame(self.command_queue.get_timestamp_period())
            {
                self.gui.report_profiling_data_simulation(simulation_profiling_data);
            } else {
                break;
            }
        }
    }

    fn draw(&mut self, event_loop_proxy: &winit::event_loop::EventLoopProxy<ApplicationEvent>) {
        let window_size = self.window.inner_size();
        if window_size.width == 0 || window_size.height == 0 {
            return;
        } else if window_size != self.screen.resolution() {
            self.window_resize(window_size);
        }

        let Some(frame) = self.screen.acquire_surface_texture(&self.device, &self.window_surface) else {
            return;
        };

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Encoder: Frame Main"),
        });

        update_global_ubo(
            &mut self.global_ubo,
            &self.command_queue,
            self.camera.fill_global_uniform_buffer(self.screen.aspect_ratio()),
            self.simulation_controller.timer().fill_global_uniform_buffer(),
            self.scene_renderer.fill_global_uniform_buffer(&self.scene),
            self.screen.fill_global_uniform_buffer(),
        );

        crate::wgpu_profiler!("scene", self.profiler_rendering, &mut encoder, &self.device, {
            self.scene_renderer.draw(
                &self.scene,
                &mut self.profiler_rendering,
                &self.device,
                &mut encoder,
                &self.pipeline_manager,
                &self.hdr_backbuffer,
                self.screen.depthbuffer(),
                self.global_bindings.bind_group(),
            );
        });

        crate::wgpu_profiler!("tonemap", self.profiler_rendering, &mut encoder, &self.device, {
            self.hdr_backbuffer
                .tonemap(&self.screen.backbuffer(), &mut encoder, &self.pipeline_manager);
        });

        self.screenshot_recorder.capture_screenshot(&mut self.screen, &self.device, &mut encoder);

        crate::wgpu_profiler!("gui", self.profiler_rendering, &mut encoder, &self.device, {
            self.gui.draw(
                &self.device,
                &self.window,
                &mut encoder,
                &self.command_queue,
                self.screen.backbuffer(),
                &mut self.simulation_controller,
                &mut self.scene_renderer,
                &mut self.scene,
                event_loop_proxy,
            );
        });

        crate::wgpu_profiler!("copy to swapchain", self.profiler_rendering, &mut encoder, &self.device, {
            self.screen.copy_to_swapchain(&frame, &mut encoder, &self.pipeline_manager);
        });
        self.profiler_rendering.resolve_queries(&mut encoder);
        self.command_queue.submit(Some(encoder.finish()));
        self.screen.end_frame_present(frame, &self.device);
        self.simulation_controller.on_frame_submitted();

        self.profiler_rendering.end_frame().unwrap();
    }
}

fn main() {
    env_logger::init_from_env(env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "warn,blub=info"));
    let event_loop = EventLoop::<ApplicationEvent>::with_user_event().build().expect("event loop");
    let application = futures::executor::block_on(Application::new(&event_loop));
    application.run(event_loop);
}
