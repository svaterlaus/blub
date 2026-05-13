use crate::simulation_controller::{SimulationController, SimulationControllerStatus};
use crate::{
    render_output::screen::Screen,
    simulation::{HybridFluid, SolverConfig, SolverStatisticSample},
    ApplicationEvent,
};
use crate::{
    renderer::{FluidRenderingMode, SceneRenderer, VolumeVisualizationMode},
    scene::Scene,
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::Duration,
};
use strum::IntoEnumIterator;
use egui_wgpu::{Renderer as EguiWgpuRenderer, RendererOptions, ScreenDescriptor};
use egui_winit::State as EguiWinitState;
use wgpu_profiler::GpuTimerQueryResult;
use winit::event_loop::EventLoopProxy;

mod custom_widgets;

const SCENE_DIRECTORY: &str = "scenes";

fn list_scene_files() -> Vec<PathBuf> {
    let files: Vec<PathBuf> = std::fs::read_dir(SCENE_DIRECTORY)
        .expect(&format!("Scene directory \"{}\" not present!", SCENE_DIRECTORY))
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.path())
        .filter(|path| path.extension().unwrap_or_default() == "json")
        .collect();

    if files.len() == 0 {
        panic!("No scene files found scene directory \"{}\"", SCENE_DIRECTORY);
    }

    files
}

pub struct GUIState {
    fast_forward_length_seconds: f32,
    video_fps: i32,
    selected_scene_idx: usize,
    known_scene_files: Vec<PathBuf>,
    wait_for_vblank: bool,

    profiling_data_rendering: Vec<GpuTimerQueryResult>,
    profiling_data_simulation: Vec<GpuTimerQueryResult>,

    show_profiling_data_rendering: bool,
    show_profiling_data_simulation: bool,
}

pub struct GUI {
    egui_ctx: egui::Context,
    egui_winit: EguiWinitState,
    egui_wgpu: EguiWgpuRenderer,

    state: GUIState,
}

impl GUI {
    pub fn new(device: &wgpu::Device, window: &winit::window::Window) -> Self {
        let mut style = egui::Style::default();
        style.visuals.code_bg_color = egui::Color32::from_rgb(64, 64, 100);

        let egui_ctx = egui::Context::default();

        egui_ctx.global_style_mut(|s| {
            *s = style.clone();
        });

        let egui_wgpu = EguiWgpuRenderer::new(device, Screen::FORMAT_BACKBUFFER, RendererOptions::default());
        let egui_winit = EguiWinitState::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            Some(device.limits().max_texture_dimension_2d as usize),
        );

        GUI {
            egui_ctx,
            egui_winit,
            egui_wgpu,
            state: GUIState {
                fast_forward_length_seconds: 5.0,
                video_fps: 60,
                selected_scene_idx: 0,
                known_scene_files: list_scene_files(),
                wait_for_vblank: Screen::DEFAULT_PRESENT_MODE == wgpu::PresentMode::Fifo,

                profiling_data_rendering: Vec::new(),
                profiling_data_simulation: Vec::new(),
                show_profiling_data_rendering: false,
                show_profiling_data_simulation: false,
            },
        }
    }

    pub fn on_window_event(&mut self, window: &winit::window::Window, event: &winit::event::WindowEvent) -> egui_winit::EventResponse {
        self.egui_winit.on_window_event(window, event)
    }

    pub fn selected_scene(&self) -> &PathBuf {
        &self.state.known_scene_files[self.state.selected_scene_idx]
    }

    fn setup_ui_timer(
        ui: &mut egui::Ui,
        state: &mut GUIState,
        simulation_controller: &SimulationController,
        event_loop_proxy: &EventLoopProxy<ApplicationEvent>,
    ) {
        ui.heading(format!(
            "{:3.2}ms, FPS: {:3.2}",
            simulation_controller.timer().duration_last_frame().as_secs_f64() * 1000.0,
            1000.0 / 1000.0 / simulation_controller.timer().duration_last_frame().as_secs_f64()
        ));

        let frame_times = simulation_controller
            .timer()
            .duration_last_frame_history()
            .iter()
            .map(|d| d.as_secs_f32() * 1000.0)
            .collect::<Vec<f32>>();
        custom_widgets::plot_barchart(
            ui,
            egui::vec2(ui.available_size_before_wrap().x, 40.0),
            &frame_times,
            frame_times.iter().cloned().fold(0.0, f32::max),
            "ms",
            1,
        );

        if ui.checkbox(&mut state.wait_for_vblank, "wait for vsync").clicked() {
            let present_mode = match state.wait_for_vblank {
                true => wgpu::PresentMode::Fifo,
                false => wgpu::PresentMode::Mailbox,
            };
            event_loop_proxy.send_event(ApplicationEvent::ChangePresentMode(present_mode)).unwrap();
        }
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("num simulation steps current frame:");
            ui.strong(format!(
                "{}",
                simulation_controller.timer().num_simulation_steps_performed_for_current_frame()
            ));
        });

        egui::Grid::new("timers").show(ui, |ui| {
            if let SimulationControllerStatus::RecordingWithFixedFrameLength { .. } = simulation_controller.status() {
                ui.colored_label(
                    egui::Color32::RED,
                    format!(
                        "OFFLINE RECORDING ({:.2}fps)",
                        1.0 / simulation_controller.timer().frame_delta().as_secs_f64()
                    ),
                );
            } else {
                ui.label("rendered time:");
                ui.strong(format!(
                    "{:.2}",
                    simulation_controller.timer().total_render_time().as_secs_f64()
                ));
            }
            ui.end_row();

            ui.label("simulated time:");
            ui.strong(format!("{:.2}", simulation_controller.timer().total_simulated_time().as_secs_f64()));
        });
    }

    fn setup_ui_solver_stats(ui: &mut egui::Ui, stats: &VecDeque<SolverStatisticSample>, max_iterations: i32, error_tolerance: f32) {
        let newest_sample = match stats.back() {
            Some(&sample) => sample,
            None => Default::default(),
        };
        ui.horizontal(|ui| {
            custom_widgets::plot_barchart(
                ui,
                egui::vec2(240.0, 40.0),
                &stats.iter().map(|sample| sample.error).collect::<Vec<f32>>(),
                error_tolerance * 3.0,
                "",
                4,
            );
            ui.vertical(|ui| {
                ui.add(egui::Label::new("max residual error").extend());
                ui.label(format!("{}", newest_sample.error));
            });
        });
        ui.horizontal(|ui| {
            custom_widgets::plot_barchart(
                ui,
                egui::vec2(240.0, 40.0),
                &stats.iter().map(|sample| sample.iteration_count as f32).collect::<Vec<f32>>(),
                max_iterations as f32,
                "",
                0,
            );
            ui.vertical(|ui| {
                ui.add(egui::Label::new("# solver iterations").extend());
                ui.label(format!("{}", newest_sample.iteration_count));
            });
        });
    }

    fn setup_ui_solver_config(ui: &mut egui::Ui, config: &mut SolverConfig) {
        egui::Grid::new("solver config").show(ui, |ui| {
            ui.label("error tolerance");
            ui.add(egui::Slider::new(&mut config.error_tolerance, 0.0001..=1.0).text(""));
            ui.end_row();

            ui.label("max iteration count");
            ui.add(egui::Slider::new(&mut config.max_num_iterations, 2..=128).text(""));
            ui.end_row();

            ui.label("error check frequency count");
            ui.add(egui::Slider::new(&mut config.error_check_frequency, 1..=config.max_num_iterations).text(""));
            ui.end_row();
        });
    }

    fn setup_ui_solver(ui: &mut egui::Ui, fluid: &mut HybridFluid) {
        {
            ui.label("pressure solver, primary (via velocity)");
            let max_num_iterations = fluid.pressure_solver_config_velocity().max_num_iterations;
            let error_tolerance = fluid.pressure_solver_config_velocity().error_tolerance;
            Self::setup_ui_solver_stats(ui, fluid.pressure_solver_stats_velocity(), max_num_iterations, error_tolerance);
            //Self::setup_ui_solver_config(ui, fluid.pressure_solver_config_velocity());
        }
        ui.separator();
        {
            ui.label("pressure solver, secondary (via density)");
            let max_num_iterations = fluid.pressure_solver_config_density().max_num_iterations;
            let error_tolerance = fluid.pressure_solver_config_density().error_tolerance;
            Self::setup_ui_solver_stats(ui, fluid.pressure_solver_stats_density(), max_num_iterations, error_tolerance);
            //Self::setup_ui_solver_config(ui, fluid.pressure_solver_config_density());
        }
        // One config for both
        ui.separator();
        {
            Self::setup_ui_solver_config(ui, fluid.pressure_solver_config_density());
            *fluid.pressure_solver_config_velocity() = *fluid.pressure_solver_config_density()
        }
    }

    fn setup_ui_simulation_control(
        ui: &mut egui::Ui,
        state: &mut GUIState,
        simulation_controller: &mut SimulationController,
        event_loop_proxy: &EventLoopProxy<ApplicationEvent>,
    ) {
        ui.horizontal(|ui| {
            if ui.button("Reset").clicked() {
                event_loop_proxy.send_event(ApplicationEvent::ResetScene).unwrap();
            }
            if ui
                .button(if simulation_controller.status() == SimulationControllerStatus::Paused {
                    "Continue  (Space)"
                } else {
                    "Pause  (Space)"
                })
                .clicked()
            {
                simulation_controller.pause_or_resume();
            }
        });

        ui.horizontal(|ui| {
            ui.label("total num simulation steps:");
            ui.strong(format!("{}", simulation_controller.timer().num_simulation_steps_performed()));
        });

        ui.separator();

        egui::Grid::new("simulation controls").show(ui, |ui| {
            ui.label("target simulation time (s)");
            let mut simulation_time_seconds = simulation_controller.simulation_stop_time.as_secs_f32();
            ui.add(egui::DragValue::new(&mut simulation_time_seconds).speed(0.1));
            simulation_controller.simulation_stop_time = std::time::Duration::from_secs_f32(simulation_time_seconds);
            ui.end_row();

            ui.label("simulation steps per second");
            let mut simulation_steps_per_second = simulation_controller.simulation_steps_per_second() as i32;
            ui.add(egui::DragValue::new(&mut simulation_steps_per_second).speed(10.0));
            simulation_controller.set_simulation_steps_per_second(simulation_steps_per_second.max(20).min(60 * 20) as u64);
            ui.end_row();

            ui.label("time scale");
            ui.add(
                egui::DragValue::new(&mut simulation_controller.time_scale)
                    .speed(0.05)
                    .clamp_range(0.01..=100.0),
            );
            ui.end_row();
        });

        ui.separator();

        ui.horizontal(|ui| {
            let min_jump = 1.0 / simulation_controller.simulation_steps_per_second() as f32;
            state.fast_forward_length_seconds = state.fast_forward_length_seconds.max(min_jump);
            ui.add(
                egui::DragValue::new(&mut state.fast_forward_length_seconds)
                    .speed(0.01)
                    .clamp_range(min_jump..=120.0),
            );
            if ui.button("Fast Forward").clicked() {
                event_loop_proxy
                    .send_event(ApplicationEvent::FastForwardSimulation(Duration::from_secs_f32(
                        state.fast_forward_length_seconds,
                    )))
                    .unwrap();
            }
            ui.label(format!("last jump took {:?}", simulation_controller.computation_time_last_fast_forward()));
        });

        if let SimulationControllerStatus::RecordingWithFixedFrameLength { .. } = simulation_controller.status() {
            if ui.button("End Recording").clicked() {
                simulation_controller.pause_or_resume();
            }
        } else {
            ui.horizontal(|ui| {
                if ui.button("Reset & Record Video").clicked() {
                    event_loop_proxy
                        .send_event(ApplicationEvent::ResetAndStartRecording {
                            recording_fps: state.video_fps as f64,
                        })
                        .unwrap();
                }

                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut state.video_fps).clamp_range(10.0..=300.0));
                    ui.label("video fps")
                });
            });
        }
    }

    fn setup_ui_scene_settings(ui: &mut egui::Ui, state: &mut GUIState, scene: &mut Scene, event_loop_proxy: &EventLoopProxy<ApplicationEvent>) {
        ui.spacing_mut().slider_width = 250.0;
        ui.horizontal(|ui| {
            ui.label("volume resolution:");
            let grid_dim = scene.config().fluid.grid_dimension;
            ui.strong(format!("{}x{}x{}", grid_dim.x, grid_dim.y, grid_dim.z));
        });
        ui.horizontal(|ui| {
            ui.label("num particles:");
            ui.strong(format!("{}", scene.num_active_particles()));
        });
        ui.separator();
        egui::ComboBox::from_label("Scene Selection")
            .selected_text(format!(
                "{:?}",
                state.known_scene_files[state.selected_scene_idx].strip_prefix(SCENE_DIRECTORY).unwrap()
            ))
            .show_ui(ui, |ui| {
                for (i, scene_file) in state.known_scene_files.iter().enumerate() {
                    if ui
                        .selectable_value(
                            &mut state.selected_scene_idx,
                            i,
                            format!("{:?}", scene_file.strip_prefix(SCENE_DIRECTORY).unwrap()),
                        )
                        .clicked()
                    {
                        event_loop_proxy
                            .send_event(ApplicationEvent::LoadScene(state.known_scene_files[state.selected_scene_idx].clone()))
                            .unwrap();
                    }
                }
            });
    }

    fn setup_ui_render_settings(ui: &mut egui::Ui, scene_renderer: &mut SceneRenderer) {
        egui::Grid::new("render settings").show(ui, |ui| {
            ui.spacing_mut().slider_width = 170.0;

            ui.label("Fluid Rendering");
            egui::ComboBox::from_label("Fluid Rendering")
                .selected_text(format!("{:?}", scene_renderer.fluid_rendering_mode))
                .show_ui(ui, |ui| {
                    for mode in FluidRenderingMode::iter() {
                        ui.selectable_value(&mut scene_renderer.fluid_rendering_mode, mode, format!("{:?}", mode));
                    }
                });
            ui.end_row();

            ui.label("Particle Radius Factor");
            ui.add(egui::Slider::new(&mut scene_renderer.particle_radius_factor, 0.0..=1.0).text(""));
            ui.end_row();

            ui.label("Volume Visualization");
            egui::ComboBox::from_label("Volume Visualization")
                .selected_text(format!("{:?}", scene_renderer.volume_visualization))
                .show_ui(ui, |ui| {
                    for mode in VolumeVisualizationMode::iter() {
                        ui.selectable_value(&mut scene_renderer.volume_visualization, mode, format!("{:?}", mode));
                    }
                });
            ui.end_row();

            ui.checkbox(&mut scene_renderer.enable_voxel_visualization, "Voxel Visualization");
            ui.end_row();

            ui.label("Velocity Visualization Scale");
            ui.add(
                egui::Slider::new(&mut scene_renderer.velocity_visualization_scale, 0.001..=5.0)
                    .logarithmic(true)
                    .text(""),
            );
        });
        ui.checkbox(&mut scene_renderer.enable_mesh_rendering, "Render meshes");
        ui.checkbox(&mut scene_renderer.enable_box_lines, "Show Fluid Domain Bounds");
    }

    fn setup_ui_profiler(ui: &mut egui::Ui, profiling_data: &[GpuTimerQueryResult], levels_default_open: i32) {
        for scope in profiling_data.iter() {
            let time = scope
                .time
                .as_ref()
                .map(|t| format!("{:.3}ms", (t.end - t.start) * 1000.0))
                .unwrap_or_else(|| "—".to_owned());
            if scope.nested_queries.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(&scope.label);
                    ui.with_layout(egui::Layout::default().with_cross_align(egui::Align::Max), |ui| {
                        ui.label(time);
                    });
                });
            } else {
                egui::CollapsingHeader::new(format!("{}  -  {}", scope.label, time))
                    .id_source(&scope.label)
                    .default_open(levels_default_open > 0)
                    .show(ui, |ui| Self::setup_ui_profiler(ui, &scope.nested_queries, levels_default_open - 1));
            }
            ui.end_row();
        }
    }

    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        window: &winit::window::Window,
        encoder: &mut wgpu::CommandEncoder,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        simulation_controller: &mut SimulationController,
        scene_renderer: &mut SceneRenderer,
        scene: &mut Scene,
        event_loop_proxy: &EventLoopProxy<ApplicationEvent>,
    ) {
        let raw_input = self.egui_winit.take_egui_input(window);
        let full_output = self.egui_ctx.run(raw_input, |ctx| {
            egui::Window::new("Blub")
                .default_size([340.0, 700.0])
                .resizable(true)
                .scroll([true, true])
                .title_bar(false)
                .show(ctx, |ui| {
                    Self::setup_ui_timer(ui, &mut self.state, simulation_controller, event_loop_proxy);

                    egui::CollapsingHeader::new("Solver").show(ui, |ui| {
                        Self::setup_ui_solver(ui, scene.fluid_mut());
                        ui.separator();
                        ui.add(
                            egui::Slider::new(&mut scene.fluid_mut().dynamic_settings().particle_rebinning_step_frequency, 0..=300)
                                .text("particle binning frequency"),
                        );
                    });
                    egui::CollapsingHeader::new("Simulation Controller & Recording")
                        .default_open(true)
                        .show(ui, |ui| {
                            Self::setup_ui_simulation_control(ui, &mut self.state, simulation_controller, event_loop_proxy);
                        });
                    egui::CollapsingHeader::new("Scene Settings").default_open(true).show(ui, |ui| {
                        Self::setup_ui_scene_settings(ui, &mut self.state, scene, event_loop_proxy);
                    });
                    egui::CollapsingHeader::new("Rendering Settings").default_open(true).show(ui, |ui| {
                        Self::setup_ui_render_settings(ui, scene_renderer);
                    });
                    if let Some(_) = egui::CollapsingHeader::new("Profiler - Single Simulation Frame")
                        .default_open(false)
                        .show(ui, |ui| {
                            if ui.button("Write Chrometrace").clicked() {
                                let filename = Path::new("simulation-trace.json");
                                info!("Writing chrome trace file to {:?}", filename);
                                wgpu_profiler::chrometrace::write_chrometrace(filename, &self.state.profiling_data_simulation)
                                    .expect("Failed to write chrometrace");
                            }
                            Self::setup_ui_profiler(ui, &self.state.profiling_data_simulation, 2);
                        })
                        .body_returned
                    {
                        self.state.show_profiling_data_simulation = true;
                    } else {
                        self.state.show_profiling_data_simulation = false;
                    }
                    if let Some(_) = egui::CollapsingHeader::new("Profiler - Rendering")
                        .default_open(false)
                        .show(ui, |ui| {
                            if ui.button("Write Chrometrace").clicked() {
                                let filename = Path::new("rendering-trace.json");
                                info!("Writing chrome trace file to {:?}", filename);
                                wgpu_profiler::chrometrace::write_chrometrace(filename, &self.state.profiling_data_rendering)
                                    .expect("Failed to write chrometrace");
                            }
                            Self::setup_ui_profiler(ui, &self.state.profiling_data_rendering, 4);
                        })
                        .body_returned
                    {
                        self.state.show_profiling_data_rendering = true;
                    } else {
                        self.state.show_profiling_data_rendering = false;
                    }
                });
        });

        self.egui_winit.handle_platform_output(window, full_output.platform_output);

        for (id, image_delta) in &full_output.textures_delta.set {
            self.egui_wgpu.update_texture(device, queue, *id, image_delta);
        }

        let jobs = self.egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [window.inner_size().width.max(1), window.inner_size().height.max(1)],
            pixels_per_point: window.scale_factor() as f32,
        };

        let callback_buffers = self.egui_wgpu.update_buffers(device, queue, encoder, &jobs, &screen_descriptor);
        if !callback_buffers.is_empty() {
            queue.submit(callback_buffers);
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.egui_wgpu.render(&mut pass.forget_lifetime(), &jobs, &screen_descriptor);
        }

        for id in &full_output.textures_delta.free {
            self.egui_wgpu.free_texture(id);
        }
    }

    pub fn report_profiling_data_rendering(&mut self, profiling_data_rendering: Vec<GpuTimerQueryResult>) {
        self.state.profiling_data_rendering = profiling_data_rendering;
    }
    pub fn report_profiling_data_simulation(&mut self, profiling_data_simulation: Vec<GpuTimerQueryResult>) {
        self.state.profiling_data_simulation = profiling_data_simulation;
    }
    pub fn show_profiling_data_simulation(&self) -> bool {
        self.state.show_profiling_data_simulation
    }
    pub fn show_profiling_data_rendering(&self) -> bool {
        self.state.show_profiling_data_rendering
    }
}
