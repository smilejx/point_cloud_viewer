// Copyright 2016 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use byteorder::{LittleEndian, ByteOrder};
use errors::*;
use math::{CuboidLike, Cuboid, Cube, Matrix4f, Vector3f, Vector2f, Frustum};
use proto;
use protobuf;
use std::cmp;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::PathBuf;
use walkdir;

mod node;

pub use self::node::{Node, NodeIterator, NodeId, NodeWriter, ChildIndex};

pub const CURRENT_VERSION: i32 = 6;

#[derive(Debug)]
pub struct VisibleNode {
    pub id: NodeId,
    pub level_of_detail: i32,
    pixels: Vector2f,
}

#[derive(Debug)]
pub struct NodesToBlob {
    pub id: NodeId,
    pub level_of_detail: i32,
}

// TODO(hrapp): something is funky here. "r" is smaller on screen than "r4" in many cases, though
// that is impossible.
fn project(m: &Matrix4f, p: &Vector3f) -> Vector3f {
    let d = 1. / (m[0][3] * p.x + m[1][3] * p.y + m[2][3] * p.z + m[3][3]);
    Vector3f::new((m[0][0] * p.x + m[1][0] * p.y + m[2][0] * p.z + m[3][0]) * d,
                  (m[0][1] * p.x + m[1][1] * p.y + m[2][1] * p.z + m[3][1]) * d,
                  (m[0][2] * p.x + m[1][2] * p.y + m[2][2] * p.z + m[3][2]) * d)
}

fn size_in_pixels(bounding_cube: &Cube, matrix: &Matrix4f, width: i32, height: i32) -> Vector2f {
    // z is unused here.
    let min = bounding_cube.min();
    let max = bounding_cube.max();
    let mut rv = Cuboid::new();
    for p in &[Vector3f::new(min.x, min.y, min.z),
               Vector3f::new(max.x, min.y, min.z),
               Vector3f::new(min.x, max.y, min.z),
               Vector3f::new(max.x, max.y, min.z),
               Vector3f::new(min.x, min.y, max.z),
               Vector3f::new(max.x, min.y, max.z),
               Vector3f::new(min.x, max.y, max.z),
               Vector3f::new(max.x, max.y, max.z)] {
        rv.update(&project(matrix, &p));
    }
    Vector2f::new((rv.max().x - rv.min().x) * (width as f32) / 2.,
                  (rv.max().y - rv.min().y) * (height as f32) / 2.)
}

#[derive(Debug)]
pub struct Octree {
    directory: PathBuf,
    // Maps from node id to number of points.
    nodes: HashMap<NodeId, u64>,
    bounding_cube: Cube,
}

#[derive(Debug)]
pub enum UseLod {
    No,
    Yes,
}

impl Octree {
    pub fn new(directory: PathBuf) -> Result<Self> {
        // We used to use JSON earlier.
        if directory.join("meta.json").exists() {
            return Err(ErrorKind::InvalidVersion(3).into());
        }

        let meta = {
            let mut reader = File::open(&directory.join("meta.pb"))?;
            protobuf::parse_from_reader::<proto::Meta>(&mut reader).chain_err(|| "Could not parse meta.pb")?
        };

        if meta.get_version() != CURRENT_VERSION {
            return Err(ErrorKind::InvalidVersion(meta.get_version()).into());
        }

        let bounding_cube = {
            let min = meta.get_bounding_cube().get_min();
            Cube::new(Vector3f::new(min.get_x(), min.get_y(), min.get_z()),
                      meta.get_bounding_cube().get_edge_length())
        };

        let mut nodes = HashMap::new();
        for entry in walkdir::WalkDir::new(&directory).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.file_name().is_none() {
                continue;
            }
            let file_name = path.file_name().unwrap();
            let file_name_str = file_name.to_str().unwrap();
            if !file_name_str.starts_with("r") && file_name_str.ends_with(".xyz") {
                continue;
            }
            let num_points = fs::metadata(path).unwrap().len() / 12;
            nodes.insert(NodeId::from_string(path.file_stem()
                             .unwrap()
                             .to_str()
                             .unwrap()
                             .to_owned()),
                         num_points);
        }

        Ok(Octree {
            directory: directory.into(),
            nodes: nodes,
            bounding_cube: bounding_cube,
        })
    }

    pub fn get_visible_nodes(&self,
                             projection_matrix: &Matrix4f,
                             width: i32,
                             height: i32,
                             use_lod: UseLod)
                             -> Vec<VisibleNode> {
        let frustum = Frustum::from_matrix(projection_matrix);
        let mut open = vec![Node::root_with_bounding_cube(self.bounding_cube.clone())];

        let mut visible = Vec::new();
        while !open.is_empty() {
            let node_to_explore = open.pop().unwrap();
            let maybe_num_points = self.nodes.get(&node_to_explore.id);
            if maybe_num_points.is_none() || !frustum.intersects(&node_to_explore.bounding_cube) {
                continue;
            }
            let num_points = *maybe_num_points.unwrap();

            let pixels = size_in_pixels(&node_to_explore.bounding_cube,
                                        projection_matrix,
                                        width,
                                        height);
            let visible_pixels = pixels.x * pixels.y;
            const MIN_PIXELS_SQ: f32 = 120.;
            const MIN_PIXELS_SIDE: f32 = 12.;
            if pixels.x < MIN_PIXELS_SIDE || pixels.y < MIN_PIXELS_SIDE ||
               visible_pixels < MIN_PIXELS_SQ {
                continue;
            }

            let level_of_detail = match use_lod {
                UseLod::No => 1,
                UseLod::Yes => {
                    // Simple heuristic: keep one point for every four pixels.
                    cmp::max(1, ((num_points as f32) / (visible_pixels / 4.)) as i32)
                }
            };

            for child_index in 0..8 {
                open.push(node_to_explore.get_child(ChildIndex::from_u8(child_index)))
            }

            visible.push(VisibleNode {
                id: node_to_explore.id,
                level_of_detail: level_of_detail,
                pixels: pixels,
            });
        }

        visible.sort_by(|a, b| {
            let size_a = a.pixels.x * a.pixels.y;
            let size_b = b.pixels.x * b.pixels.y;
            size_b.partial_cmp(&size_a).unwrap()
        });
        visible
    }

    pub fn get_nodes_as_binary_blob(&self, nodes: &[NodesToBlob]) -> Result<(usize, Vec<u8>)> {
        const NUM_BYTES_PER_POINT: usize = 4 * 3 + 4;

        let mut num_points = 0;
        let mut rv = Vec::new();
        for node in nodes {
            let points: Vec<_> = NodeIterator::from_disk(&self.directory, &node.id)?.collect();
            let num_points_for_lod =
                (points.len() as f32 / node.level_of_detail as f32).ceil() as usize;

            num_points += num_points_for_lod;
            let mut pos = rv.len();
            rv.resize(pos + 4 + NUM_BYTES_PER_POINT * num_points_for_lod, 0u8);
            LittleEndian::write_u32(&mut rv[pos..],
                                    (num_points_for_lod * NUM_BYTES_PER_POINT) as u32);
            pos += 4;

            // Put positions.
            for (idx, p) in points.iter().enumerate() {
                if idx % node.level_of_detail as usize != 0 {
                    continue;
                }
                LittleEndian::write_f32(&mut rv[pos..], p.position.x);
                pos += 4;
                LittleEndian::write_f32(&mut rv[pos..], p.position.y);
                pos += 4;
                LittleEndian::write_f32(&mut rv[pos..], p.position.z);
                pos += 4;
            }

            // Put colors.
            for (idx, p) in points.iter().enumerate() {
                if idx % node.level_of_detail as usize != 0 {
                    continue;
                }
                rv[pos] = p.r;
                pos += 1;
                rv[pos] = p.g;
                pos += 1;
                rv[pos] = p.b;
                pos += 1;
                rv[pos] = 255;
                pos += 1;
            }
        }
        assert_eq!(4 * nodes.len() + NUM_BYTES_PER_POINT * num_points, rv.len());
        Ok((num_points, rv))
    }
}
